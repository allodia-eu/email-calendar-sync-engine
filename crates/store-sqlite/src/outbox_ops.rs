//! The outbox half of the store: enqueue (idempotent), claim (dependency,
//! resource, and lease-expiry filtering), mark, and op-state read.
//!
//! Claim replays the reference store's algorithm over the account's ops loaded in
//! id order, so the runnable set is identical: skip ops with unmet dependencies,
//! and never lease two ops sharing a resource — neither against an op already
//! live in flight, nor twice within one claim round.

use std::collections::{HashMap, HashSet};

use engine_core::{
    ids::AccountId,
    time::UtcDateTime,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use engine_store::{
    ClaimRejection, FenceToken, LeasedPendingOp, OpLease, PendingOpClaim, PendingOpState, Result,
    StoreError, WorkerId,
};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::Value;

use crate::convert;

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
             (account, idempotency_key, resource_key, depends_on, payload, state, token, lease_expiry)
         VALUES (?1, ?2, ?3, ?4, ?5, 'Pending', 0, NULL)",
        (
            account.as_str(),
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
        let claimable = op.state == PendingOpState::Pending
            || (op.state == PendingOpState::InFlight && !convert::is_live(op.lease_expiry, now));
        if !claimable {
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
/// Reads only what the decision needs — the op, its dependencies, and the ops
/// sharing its resource — rather than the account's whole outbox: an account
/// accumulates settled ops forever (they are the idempotency record), and a write
/// must not get slower for every write that came before it.
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
    // The batch claim's own predicate: a fresh op, or one whose lease died under it.
    match op.state {
        PendingOpState::Pending => {}
        PendingOpState::InFlight if !convert::is_live(op.lease_expiry, now) => {}
        PendingOpState::InFlight => {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Busy));
        }
        PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::NeedsConfirmation => {
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

/// Whether every op in `depends_on` has reached terminal success. Scoped to
/// `account`, so a dependency naming another account's op reads as unmet rather than
/// as satisfied by work this account never did.
fn dependencies_met(
    tx: &Transaction<'_>,
    account: &str,
    depends_on: &[PendingOpId],
) -> Result<bool> {
    for dep in depends_on {
        let id = convert::op_id_to_i64(*dep)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM pending_op WHERE account = ?1 AND id = ?2",
                (account, id),
                |r| r.get(0),
            )
            .optional()
            .map_err(convert::backend)?;
        let met = match state {
            Some(text) => convert::parse_state(&text)?.is_success(),
            None => false,
        };
        if !met {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a **different** op of this account holds `resource` under a live lease.
fn resource_held_elsewhere(
    tx: &Transaction<'_>,
    account: &str,
    resource: &str,
    op_id: i64,
    now: UtcDateTime,
) -> Result<bool> {
    let mut stmt = tx
        .prepare(
            "SELECT lease_expiry FROM pending_op
             WHERE account = ?1 AND resource_key = ?2 AND id != ?3 AND state = 'InFlight'",
        )
        .map_err(convert::backend)?;
    let expiries = stmt
        .query_map((account, resource, op_id), |r| {
            r.get::<_, Option<String>>(0)
        })
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;
    for expiry in expiries {
        if convert::is_live(convert::parse_opt_instant(expiry)?, now) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Records a claimed op's outcome, gated by its lease token.
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the op was re-claimed (token
/// superseded), or [`StoreError::Backend`] on a backend failure.
pub(crate) fn mark(
    conn: &mut Connection,
    op_id: PendingOpId,
    token: u64,
    outcome: &PendingOutcome,
) -> Result<()> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let id = convert::op_id_to_i64(op_id)?;
    let current: Option<i64> = tx
        .query_row("SELECT token FROM pending_op WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(convert::backend)?;
    let current_matches = match current {
        Some(stored) => convert::generation_from_i64(stored)? == token,
        None => false,
    };
    if !current_matches {
        return Err(StoreError::StaleLease);
    }

    let state = match outcome {
        PendingOutcome::Succeeded { .. } => PendingOpState::Succeeded,
        PendingOutcome::Failed { .. } => PendingOpState::Failed,
        PendingOutcome::NeedsConfirmation { .. } => PendingOpState::NeedsConfirmation,
    };
    tx.execute(
        "UPDATE pending_op SET state = ?1, lease_expiry = NULL WHERE id = ?2",
        (convert::state_to_text(state), id),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// The current lifecycle state of an op, or `None` if unknown.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn pending_op_state(
    conn: &Connection,
    op_id: PendingOpId,
) -> Result<Option<PendingOpState>> {
    let id = convert::op_id_to_i64(op_id)?;
    let state: Option<String> = conn
        .query_row("SELECT state FROM pending_op WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(convert::backend)?;
    match state {
        Some(text) => Ok(Some(convert::parse_state(&text)?)),
        None => Ok(None),
    }
}

/// One op loaded for the claim decision, with its envelope fields parsed.
struct LoadedOp {
    id: i64,
    idempotency_key: String,
    resource_key: String,
    depends_on: Vec<PendingOpId>,
    payload: Value,
    state: PendingOpState,
    token: u64,
    lease_expiry: Option<UtcDateTime>,
}

impl LoadedOp {
    /// Rebuilds the public [`PendingOp`] envelope to hand back in a lease.
    fn to_pending_op(&self) -> Result<PendingOp> {
        Ok(PendingOp {
            idempotency_key: IdempotencyKey::new(self.idempotency_key.clone())
                .map_err(convert::backend)?,
            depends_on: self.depends_on.clone(),
            resource_key: ResourceKey::new(self.resource_key.clone()).map_err(convert::backend)?,
            payload: self.payload.clone(),
        })
    }
}

/// The `SELECT` list every op load shares, in [`LoadedOp`]'s field order.
const OP_COLUMNS: &str =
    "id, idempotency_key, resource_key, depends_on, payload, state, token, lease_expiry";

/// Loads one op by id, scoped to `account` so an id from another account reads as
/// absent rather than as someone else's work.
fn load_one_op(tx: &Transaction<'_>, account: &str, id: i64) -> Result<Option<LoadedOp>> {
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE account = ?1 AND id = ?2");
    let raw = tx
        .query_row(&sql, (account, id), read_op_row)
        .optional()
        .map_err(convert::backend)?;
    raw.map(parse_op_row).transpose()
}

/// Loads an account's ops in id order, parsing the stored envelope columns.
fn load_account_ops(tx: &Transaction<'_>, account: &str) -> Result<Vec<LoadedOp>> {
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE account = ?1 ORDER BY id");
    let mut stmt = tx.prepare(&sql).map_err(convert::backend)?;
    let raws = stmt
        .query_map([account], read_op_row)
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    raws.into_iter().map(parse_op_row).collect()
}

/// One op row as stored, in [`OP_COLUMNS`] order.
type OpRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
);

/// Reads an op row's columns without interpreting them.
fn read_op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

/// Parses a read row's envelope columns into a [`LoadedOp`].
fn parse_op_row(raw: OpRow) -> Result<LoadedOp> {
    let (id, idempotency_key, resource_key, depends_on, payload, state, token, lease_expiry) = raw;
    Ok(LoadedOp {
        id,
        idempotency_key,
        resource_key,
        depends_on: serde_json::from_str(&depends_on).map_err(convert::backend)?,
        payload: serde_json::from_str(&payload).map_err(convert::backend)?,
        state: convert::parse_state(&state)?,
        token: convert::generation_from_i64(token)?,
        lease_expiry: convert::parse_opt_instant(lease_expiry)?,
    })
}
