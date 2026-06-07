//! The outbox half of the store: enqueue (idempotent), claim (dependency,
//! resource, and lease-expiry filtering), mark, and op-state read.
//!
//! Claim replays the reference store's algorithm over the account's ops loaded in
//! id order, so the runnable set is identical: skip ops with unmet dependencies,
//! and never lease two ops sharing a resource — neither against an op already
//! live in flight, nor twice within one claim round.

use std::collections::{HashMap, HashSet};

use engine_core::ids::AccountId;
use engine_core::time::UtcDateTime;
use engine_core::write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::Value;

use engine_store::{
    FenceToken, LeasedPendingOp, OpLease, PendingOpState, Result, StoreError, WorkerId,
};

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

/// Loads an account's ops in id order, parsing the stored envelope columns.
fn load_account_ops(tx: &Transaction<'_>, account: &str) -> Result<Vec<LoadedOp>> {
    let mut stmt = tx
        .prepare(
            "SELECT id, idempotency_key, resource_key, depends_on, payload, state, token, lease_expiry
             FROM pending_op WHERE account = ?1 ORDER BY id",
        )
        .map_err(convert::backend)?;
    let raws = stmt
        .query_map([account], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    let mut ops = Vec::with_capacity(raws.len());
    for (id, idempotency_key, resource_key, depends_on, payload, state, token, lease_expiry) in raws
    {
        ops.push(LoadedOp {
            id,
            idempotency_key,
            resource_key,
            depends_on: serde_json::from_str(&depends_on).map_err(convert::backend)?,
            payload: serde_json::from_str(&payload).map_err(convert::backend)?,
            state: convert::parse_state(&state)?,
            token: convert::generation_from_i64(token)?,
            lease_expiry: convert::parse_opt_instant(lease_expiry)?,
        });
    }
    Ok(ops)
}
