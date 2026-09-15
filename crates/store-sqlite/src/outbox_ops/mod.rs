//! The outbox half of the store: enqueue (idempotent), claim (dependency,
//! resource, backoff and lease-expiry filtering), mark, cancel, and the reads.
//!
//! Claim replays the reference store's algorithm over the account's ops loaded in
//! id order, so the runnable set is identical: skip ops with unmet dependencies,
//! skip a retry still waiting out its backoff, and never lease two ops sharing a
//! resource — neither against an op already live in flight, nor twice within one
//! claim round.

use std::collections::{HashMap, HashSet};

use engine_core::{
    ids::AccountId,
    time::UtcDateTime,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    CancelRejection, ClaimRejection, FenceToken, LeasedPendingOp, MAX_ATTEMPTS, OpLease,
    PendingOpClaim, PendingOpState, Result, StoreError, WorkerId, retry_delay,
};
use rusqlite::{Connection, OptionalExtension};

use self::row::{
    dependencies_met, is_due, is_runnable, load_account_ops, load_one_op, resource_held_elsewhere,
};
use crate::convert;

mod read;
mod row;

pub(crate) use read::{list_pending_ops, pending_op_state};

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
    let tx = conn.transaction().map_err(convert::backend)?;
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
    tx.execute(
        "INSERT INTO pending_op
             (account, kind, idempotency_key, resource_key, depends_on, payload, state, token,
              lease_expiry, attempts, next_attempt_at, failure_class, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'Pending', 0, NULL, 0, NULL, NULL, NULL)",
        (
            account.as_str(),
            convert::kind_to_text(op.kind),
            op.idempotency_key.as_str(),
            op.resource_key.as_str(),
            depends_on,
            payload,
        ),
    )
    .map_err(convert::backend)?;
    let id = tx.last_insert_rowid();
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
    let tx = conn.transaction().map_err(convert::backend)?;
    let ops = load_account_ops(&tx, account.as_str())?;

    // Dependency lookup and the set of resources held by a live in-flight op.
    let state_by_id: HashMap<i64, PendingOpState> = ops.iter().map(|o| (o.id, o.state)).collect();
    let busy: HashSet<&str> = ops
        .iter()
        .filter(|o| o.state == PendingOpState::InFlight && convert::is_live(o.lease_expiry, now))
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
    let tx = conn.transaction().map_err(convert::backend)?;

    let Some(op) = load_one_op(&tx, account.as_str(), id)? else {
        return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
    };
    // The batch claim's own predicate: a fresh op that is due, or one whose lease
    // died under it. A kind-less row is unrunnable and reads as unknown work.
    match op.state {
        _ if op.kind.is_none() => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
        }
        PendingOpState::Pending if is_due(op.next_attempt_at, now) => {}
        PendingOpState::Pending => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Backoff));
        }
        PendingOpState::InFlight if !convert::is_live(op.lease_expiry, now) => {}
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
    if !dependencies_met(&tx, account.as_str(), &op.depends_on)? {
        return Ok(PendingOpClaim::Refused(ClaimRejection::DependencyUnmet));
    }
    if resource_held_elsewhere(&tx, account.as_str(), &op.resource_key, id, now)? {
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
    tx.commit().map_err(convert::backend)?;

    let lease = OpLease::new(account.clone(), op_id, token, owner.clone(), expiry);
    Ok(PendingOpClaim::Leased(Box::new(LeasedPendingOp::new(
        op_id, pending, lease,
    ))))
}

/// Records a claimed op's outcome, gated by its lease token.
///
/// A retryable failure parks rather than settles: the state goes back to `Pending`
/// with the attempt counted and `next_attempt_at` set, so the op leaves the runnable
/// set only until its backoff elapses.
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the op was re-claimed (token
/// superseded), or [`StoreError::Backend`] on a backend failure.
pub(crate) fn mark(
    conn: &mut Connection,
    op_id: PendingOpId,
    token: u64,
    now: UtcDateTime,
    outcome: &PendingOutcome,
) -> Result<()> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let id = convert::op_id_to_i64(op_id)?;
    let current: Option<(i64, i64)> = tx
        .query_row(
            "SELECT token, attempts FROM pending_op WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(convert::backend)?;
    let Some((stored_token, stored_attempts)) = current else {
        return Err(StoreError::StaleLease);
    };
    if convert::generation_from_i64(stored_token)? != token {
        return Err(StoreError::StaleLease);
    }
    let attempts = u32::try_from(stored_attempts)
        .map_err(convert::backend)?
        .saturating_add(1);

    let (state, next_attempt_at, class, detail) = match outcome {
        PendingOutcome::Succeeded { .. } => (PendingOpState::Succeeded, None, None, None),
        PendingOutcome::Failed { class, retry_after } => {
            // A class a plain retry cannot fix settles now; so does one that has
            // used up its attempts. Everything else parks and comes back.
            if class.is_retryable() && attempts < MAX_ATTEMPTS {
                let due = now
                    .checked_add(retry_delay(attempts, *retry_after))
                    .ok_or_else(|| StoreError::Backend("retry delay overflow".to_owned()))?;
                (PendingOpState::Pending, Some(due), Some(*class), None)
            } else {
                (PendingOpState::Failed, None, Some(*class), None)
            }
        }
        PendingOutcome::NeedsConfirmation { detail } => (
            PendingOpState::NeedsConfirmation,
            None,
            None,
            Some(detail.clone()),
        ),
    };

    tx.execute(
        "UPDATE pending_op
            SET state = ?1, lease_expiry = NULL, attempts = ?2, next_attempt_at = ?3,
                failure_class = ?4, detail = ?5
          WHERE id = ?6",
        (
            convert::state_to_text(state),
            i64::from(attempts),
            next_attempt_at.map(convert::instant_to_text),
            class.map(convert::class_to_text),
            detail,
            id,
        ),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// Withdraws a queued op, settling it as `Cancelled` so nothing attempts it.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn cancel(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    now: UtcDateTime,
) -> Result<Option<CancelRejection>> {
    let id = convert::op_id_to_i64(op_id)?;
    let tx = conn.transaction().map_err(convert::backend)?;
    let Some(op) = load_one_op(&tx, account.as_str(), id)? else {
        return Ok(Some(CancelRejection::Unknown));
    };
    match op.state {
        // A dead lease is nobody's side effect: the worker that held it is gone.
        PendingOpState::Pending => {}
        PendingOpState::InFlight if !convert::is_live(op.lease_expiry, now) => {}
        PendingOpState::InFlight => return Ok(Some(CancelRejection::InFlight)),
        PendingOpState::NeedsConfirmation => {
            return Ok(Some(CancelRejection::AwaitingConfirmation));
        }
        PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::Cancelled => {
            return Ok(Some(CancelRejection::Settled));
        }
    }
    // Bump the token so a worker still holding the old lease cannot resolve it.
    let token = FenceToken::from_generation(op.token).bump();
    tx.execute(
        "UPDATE pending_op
            SET state = 'Cancelled', token = ?1, lease_expiry = NULL, next_attempt_at = NULL
          WHERE id = ?2",
        (convert::generation_to_i64(token.get())?, id),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(None)
}
