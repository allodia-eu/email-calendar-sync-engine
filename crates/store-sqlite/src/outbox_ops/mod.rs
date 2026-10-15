//! The outbox half of the store: enqueue (idempotent), claim (dependency,
//! resource and backoff filtering), the hand-over record and lease renewal, mark, and the
//! host's verbs (`host`) and reads (`read`).
//!
//! Claim replays the reference store's algorithm over the account's ops loaded in
//! id order, so the runnable set is identical: recover every dead attempt first, skip ops
//! with unmet dependencies, skip a retry still waiting out its backoff, and never lease two
//! ops sharing a resource — neither against an op already in flight, nor twice within one
//! claim round.

use std::collections::{HashMap, HashSet};

use engine_core::{
    ids::AccountId,
    time::UtcDateTime,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    ClaimRejection, FenceToken, LeasedPendingOp, OpLease, PendingOpClaim, PendingOpState, Result,
    StoreError, WorkerId,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use self::{
    recover::record,
    row::{
        dependencies_met, is_due, is_runnable, load_account_ops, load_one_op,
        resource_held_elsewhere,
    },
};
use crate::convert;

mod host;
mod read;
mod recover;
mod row;

pub(crate) use host::{cancel, confirm, retry_now};
pub(crate) use read::{list_pending_ops, pending_op_state};
pub(crate) use recover::{recover_dead, recover_interrupted};

/// Opens an outbox write transaction holding the write lock from its first statement.
///
/// Two processes can share one store file (an app and its background task). A deferred
/// transaction that read and then writes fails at once with `SQLITE_BUSY` when the other
/// process committed in between, and `busy_timeout` cannot help it; an immediate one queues on
/// `busy_timeout` instead, so a claim racing another process waits rather than erroring.
pub(super) fn begin(conn: &mut Connection) -> Result<Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(convert::backend)
}

/// Runs `write` with each commit synced to disk before it returns.
///
/// A file store runs WAL with `synchronous = NORMAL`: a commit survives the process dying but
/// can be lost to a power cut, which rolls the database back to its last sync. Three outbox
/// writes cannot be lost that way, so they pay for a sync each: an enqueue (the user's message
/// exists nowhere else the engine controls), a hand-over record (losing it makes a delivered
/// send look like one that never left, which is a second delivery), and a withdrawal (losing it
/// sends a message the user stopped).
pub(super) fn durably<R>(
    conn: &mut Connection,
    write: impl FnOnce(&mut Connection) -> Result<R>,
) -> Result<R> {
    let level: i64 = conn
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .map_err(convert::backend)?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(convert::backend)?;
    let result = write(conn);
    // Restored whatever the write did: the writer goes on to serve every other write.
    conn.pragma_update(None, "synchronous", level)
        .map_err(convert::backend)?;
    result
}

/// Durably enqueues an op, idempotent by `(account, idempotency_key)`: a repeat
/// key returns the existing id and inserts nothing.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn enqueue(
    conn: &mut Connection,
    account: &AccountId,
    op: &PendingOp,
) -> Result<PendingOpId> {
    durably(conn, |conn| enqueue_in(conn, account, op))
}

/// [`enqueue`], under whatever sync level the caller set.
fn enqueue_in(conn: &mut Connection, account: &AccountId, op: &PendingOp) -> Result<PendingOpId> {
    let tx = begin(conn)?;
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM pending_op WHERE account = ?1 AND idempotency_key = ?2",
            (account.as_str(), op.idempotency_key.as_str()),
            |r| r.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    if let Some(id) = existing {
        tx.commit().map_err(convert::backend)?;
        return convert::op_id_from_i64(id);
    }

    let depends_on = serde_json::to_string(&op.depends_on).map_err(convert::backend)?;
    let payload = serde_json::to_string(&op.payload).map_err(convert::backend)?;
    let keyword_edit = op
        .keyword_edit
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(convert::backend)?;
    tx.execute(
        "INSERT INTO pending_op
             (account, kind, idempotency_key, resource_key, depends_on, payload, state, token,
              lease_expiry, attempts, next_attempt_at, failure_class, detail, keyword_edit)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'Pending', 0, NULL, 0, NULL, NULL, NULL, ?7)",
        (
            account.as_str(),
            convert::kind_to_text(op.kind),
            op.idempotency_key.as_str(),
            op.resource_key.as_str(),
            depends_on,
            payload,
            keyword_edit,
        ),
    )
    .map_err(convert::backend)?;
    let id = tx.last_insert_rowid();
    // In the same transaction as the op, so a change is never shown without the op that makes
    // it, nor queued without being shown.
    if let Some(edit) = &op.keyword_edit {
        crate::keyword_edits::queued(&tx, account.as_str(), edit)?;
    }
    tx.commit().map_err(convert::backend)?;
    convert::op_id_from_i64(id)
}

/// Claims up to `limit` runnable ops for `account`, each leased with a fresh
/// fencing token.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn claim(
    conn: &mut Connection,
    account: &AccountId,
    owner: &WorkerId,
    now: UtcDateTime,
    expiry: UtcDateTime,
    limit: usize,
) -> Result<Vec<LeasedPendingOp>> {
    let tx = begin(conn)?;
    recover_dead(&tx, account.as_str(), now)?;
    let ops = load_account_ops(&tx, account.as_str())?;

    // Dependency lookup and the set of resources held by an op in flight, every one of
    // which is live now that the dead ones are recovered.
    let state_by_id: HashMap<i64, PendingOpState> = ops.iter().map(|o| (o.id, o.state)).collect();
    let busy: HashSet<&str> = ops
        .iter()
        .filter(|o| o.state == PendingOpState::InFlight)
        .map(|o| o.resource_key.as_str())
        .collect();

    let mut newly_leased: HashSet<&str> = HashSet::new();
    let mut result = Vec::new();
    for op in &ops {
        if result.len() >= limit {
            break;
        }
        if !is_runnable(op, now) {
            continue;
        }
        let deps_ok = op.depends_on.iter().all(|dep| {
            convert::op_id_to_i64(*dep)
                .ok()
                .and_then(|id| state_by_id.get(&id))
                .is_some_and(|state| state.is_success())
        });
        if !deps_ok {
            continue;
        }
        if busy.contains(op.resource_key.as_str()) || !newly_leased.insert(op.resource_key.as_str())
        {
            continue;
        }

        let token = FenceToken::from_generation(op.token).bump();
        tx.execute(
            "UPDATE pending_op SET token = ?1, state = 'InFlight', lease_expiry = ?2 WHERE id = ?3",
            (
                convert::generation_to_i64(token.get())?,
                convert::instant_to_text(expiry),
                op.id,
            ),
        )
        .map_err(convert::backend)?;

        let op_id = convert::op_id_from_i64(op.id)?;
        let lease = OpLease::new(account.clone(), op_id, token, owner.clone(), expiry);
        result.push(LeasedPendingOp::new(op_id, op.to_pending_op()?, lease));
    }

    tx.commit().map_err(convert::backend)?;
    Ok(result)
}

/// Claims the one op `op_id` names, under the same runnable rules as [`claim`],
/// reporting which condition refused it when it cannot be leased.
///
/// Reads only what the decision needs — the op, its dependencies, and the live
/// in-flight ops sharing its resource — rather than the account's whole outbox: an
/// account accumulates settled ops forever (they are the idempotency record), and a
/// write must not get slower for every write that came before it. Each read rides a
/// covering index (`pending_op`'s primary key, and `pending_op_held_resource` for the
/// resource probe); a query here that falls back to `account = ?` scans them all.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn claim_one(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    owner: &WorkerId,
    now: UtcDateTime,
    expiry: UtcDateTime,
) -> Result<PendingOpClaim> {
    let id = convert::op_id_to_i64(op_id)?;
    let tx = begin(conn)?;
    // Committed even when the claim refuses: what it recovered is true either way.
    let claim = claim_one_in(&tx, account, op_id, id, owner, now, expiry)?;
    tx.commit().map_err(convert::backend)?;
    Ok(claim)
}

/// [`claim_one`] inside its transaction.
fn claim_one_in(
    tx: &rusqlite::Transaction<'_>,
    account: &AccountId,
    op_id: PendingOpId,
    id: i64,
    owner: &WorkerId,
    now: UtcDateTime,
    expiry: UtcDateTime,
) -> Result<PendingOpClaim> {
    // Every dead attempt of the account, not only this op's: one of them may hold the
    // resource this op needs, and it holds nothing once recovered.
    recover_dead(tx, account.as_str(), now)?;
    let Some(op) = load_one_op(tx, account.as_str(), id)? else {
        return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
    };
    // The batch claim's own predicate: `Pending` and due. A kind-less row is unrunnable and
    // reads as unknown work.
    match op.state {
        _ if op.kind.is_none() => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
        }
        PendingOpState::Pending if is_due(op.next_attempt_at, now) => {}
        PendingOpState::Pending => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Backoff));
        }
        PendingOpState::InFlight => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Busy));
        }
        PendingOpState::Succeeded
        | PendingOpState::Failed
        | PendingOpState::Cancelled
        | PendingOpState::NeedsConfirmation => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Settled));
        }
    }
    if !dependencies_met(tx, account.as_str(), &op.depends_on)? {
        return Ok(PendingOpClaim::Refused(ClaimRejection::DependencyUnmet));
    }
    if resource_held_elsewhere(tx, account.as_str(), &op.resource_key, id, now)? {
        return Ok(PendingOpClaim::Refused(ClaimRejection::Busy));
    }

    let token = FenceToken::from_generation(op.token).bump();
    tx.execute(
        "UPDATE pending_op SET token = ?1, state = 'InFlight', lease_expiry = ?2 WHERE id = ?3",
        (
            convert::generation_to_i64(token.get())?,
            convert::instant_to_text(expiry),
            id,
        ),
    )
    .map_err(convert::backend)?;
    let pending = op.to_pending_op()?;

    let lease = OpLease::new(account.clone(), op_id, token, owner.clone(), expiry);
    Ok(PendingOpClaim::Leased(Box::new(LeasedPendingOp::new(
        op_id, pending, lease,
    ))))
}

/// Records a claimed op's outcome, gated by its lease token.
///
/// A retryable failure parks rather than settles: the state goes back to `Pending`
/// with the attempt counted and `next_attempt_at` set, so the op leaves the runnable
/// set only until its backoff elapses. The attempt that recorded a send's hand-over is
/// still heard after a recovery parked the send for confirmation under it
/// (`Store::mark_pending_op`).
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the op was re-claimed (token
/// superseded), or [`StoreError::Backend`] on a backend failure.
pub(crate) fn mark(
    conn: &mut Connection,
    lease: &OpLease,
    now: UtcDateTime,
    outcome: &PendingOutcome,
) -> Result<()> {
    let tx = begin(conn)?;
    let id = convert::op_id_to_i64(lease.op())?;
    let Some(op) = load_one_op(&tx, lease.account().as_str(), id)? else {
        return Err(StoreError::StaleLease);
    };
    let token = lease.token().get();
    let current = op.token == token;
    let handed_over =
        op.state == PendingOpState::NeedsConfirmation && op.handed_over == Some(token);
    if !current && !handed_over {
        return Err(StoreError::StaleLease);
    }
    record(&tx, &op, now, outcome)?;
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// Records that `lease`'s attempt is about to hand its message over
/// (`Store::record_hand_over`).
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if `lease` is not the op's current lease or the op is
/// not `InFlight`, or [`StoreError::Backend`] on a backend failure.
pub(crate) fn hand_over(conn: &mut Connection, lease: &OpLease) -> Result<()> {
    let token = convert::generation_to_i64(lease.token().get())?;
    let op = convert::op_id_to_i64(lease.op())?;
    let written = durably(conn, |conn| {
        conn.execute(
            "UPDATE pending_op SET handed_over = ?1
              WHERE id = ?2 AND account = ?3 AND token = ?1 AND state = 'InFlight'",
            (token, op, lease.account().as_str()),
        )
        .map_err(convert::backend)
    })?;
    if written == 0 {
        return Err(StoreError::StaleLease);
    }
    Ok(())
}

/// Extends `lease` to `expiry` (`Store::renew_pending_op_lease`).
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if `lease` is not the op's current lease or the op is
/// not `InFlight`, or [`StoreError::Backend`] on a backend failure.
pub(crate) fn renew(conn: &Connection, lease: &OpLease, expiry: UtcDateTime) -> Result<()> {
    let written = conn
        .execute(
            "UPDATE pending_op SET lease_expiry = ?1
              WHERE id = ?2 AND account = ?3 AND token = ?4 AND state = 'InFlight'",
            (
                convert::instant_to_text(expiry),
                convert::op_id_to_i64(lease.op())?,
                lease.account().as_str(),
                convert::generation_to_i64(lease.token().get())?,
            ),
        )
        .map_err(convert::backend)?;
    if written == 0 {
        return Err(StoreError::StaleLease);
    }
    Ok(())
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod restart_tests;
