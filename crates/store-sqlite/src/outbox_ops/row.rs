//! Loading `pending_op` rows and the predicates a claim decides on.
//!
//! Split from the operations themselves so the SQL that shapes a row lives in one
//! place: every load goes through [`OP_COLUMNS`] and [`parse_op_row`], so a column
//! added to the table is added once.

use engine_core::{
    time::UtcDateTime,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, ResourceKey},
};
use engine_store::{PendingOpState, Result};
use rusqlite::{OptionalExtension, Transaction};
use serde_json::Value;

use crate::convert;

/// One op loaded for the claim decision, with its envelope fields parsed.
pub(super) struct LoadedOp {
    pub(super) id: i64,
    pub(super) kind: Option<PendingOpKind>,
    pub(super) idempotency_key: String,
    pub(super) resource_key: String,
    pub(super) depends_on: Vec<PendingOpId>,
    pub(super) payload: Value,
    pub(super) state: PendingOpState,
    pub(super) token: u64,
    pub(super) lease_expiry: Option<UtcDateTime>,
    pub(super) attempts: u32,
    pub(super) next_attempt_at: Option<UtcDateTime>,
    pub(super) failure_class: Option<engine_core::error::FailureClass>,
    pub(super) detail: Option<String>,
}

impl LoadedOp {
    /// Rebuilds the public [`PendingOp`] envelope to hand back in a lease.
    ///
    /// Only ever called for a row a claim accepted, and [`is_runnable`] refuses one
    /// with no kind, so the `expect` cannot fire: a lease is never handed out for an
    /// op nothing can dispatch.
    pub(super) fn to_pending_op(&self) -> Result<PendingOp> {
        Ok(PendingOp {
            idempotency_key: IdempotencyKey::new(self.idempotency_key.clone())
                .map_err(convert::backend)?,
            kind: self.kind.expect("a claimed op has a kind"),
            depends_on: self.depends_on.clone(),
            resource_key: ResourceKey::new(self.resource_key.clone()).map_err(convert::backend)?,
            payload: self.payload.clone(),
        })
    }
}

/// Whether a parked retry's backoff has elapsed. An op with no `next_attempt_at`
/// has never failed and is due immediately.
pub(super) fn is_due(next_attempt_at: Option<UtcDateTime>, now: UtcDateTime) -> bool {
    next_attempt_at.is_none_or(|due| due <= now)
}

/// Whether an op may be leased now: fresh and due, or one whose lease died under it.
///
/// A row with no kind is never runnable. Those are the rows enqueued before v14, whose
/// payload nothing can be deserialized as; attempting one would mean guessing which
/// provider verb it was. They stay listed so a host can show and withdraw them.
pub(super) fn is_runnable(op: &LoadedOp, now: UtcDateTime) -> bool {
    if op.kind.is_none() {
        return false;
    }
    match op.state {
        PendingOpState::Pending => is_due(op.next_attempt_at, now),
        PendingOpState::InFlight => !convert::is_live(op.lease_expiry, now),
        PendingOpState::Succeeded
        | PendingOpState::Failed
        | PendingOpState::Cancelled
        | PendingOpState::NeedsConfirmation => false,
    }
}

/// The `SELECT` list every op load shares, in [`LoadedOp`]'s field order.
pub(super) const OP_COLUMNS: &str = "id, kind, idempotency_key, resource_key, depends_on, \
     payload, state, token, lease_expiry, attempts, next_attempt_at, failure_class, detail";

/// Loads one op by id, scoped to `account` so an id from another account reads as
/// absent rather than as someone else's work.
pub(super) fn load_one_op(
    tx: &Transaction<'_>,
    account: &str,
    id: i64,
) -> Result<Option<LoadedOp>> {
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE account = ?1 AND id = ?2");
    let raw = tx
        .query_row(&sql, (account, id), read_op_row)
        .optional()
        .map_err(convert::backend)?;
    raw.map(parse_op_row).transpose()
}

/// Loads an account's ops in id order, parsing the stored envelope columns.
pub(super) fn load_account_ops(tx: &Transaction<'_>, account: &str) -> Result<Vec<LoadedOp>> {
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE account = ?1 ORDER BY id");
    let mut stmt = tx.prepare(&sql).map_err(convert::backend)?;
    let raws = stmt
        .query_map([account], read_op_row)
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    raws.into_iter().map(parse_op_row).collect()
}

/// Every op in `InFlight`, across accounts, in id order.
pub(super) fn load_in_flight_ops(tx: &Transaction<'_>) -> Result<Vec<LoadedOp>> {
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE state = 'InFlight' ORDER BY id");
    let mut stmt = tx.prepare(&sql).map_err(convert::backend)?;
    let raws = stmt
        .query_map([], read_op_row)
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    raws.into_iter().map(parse_op_row).collect()
}

/// One op row as stored, in [`OP_COLUMNS`] order.
type OpRow = (
    i64,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Reads an op row's columns without interpreting them.
pub(super) fn read_op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpRow> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
    ))
}

/// Parses a read row's envelope columns into a [`LoadedOp`].
pub(super) fn parse_op_row(raw: OpRow) -> Result<LoadedOp> {
    let (
        id,
        kind,
        idempotency_key,
        resource_key,
        depends_on,
        payload,
        state,
        token,
        lease_expiry,
        attempts,
        next_attempt_at,
        failure_class,
        detail,
    ) = raw;
    Ok(LoadedOp {
        id,
        kind: convert::parse_kind(kind.as_deref())?,
        idempotency_key,
        resource_key,
        depends_on: serde_json::from_str(&depends_on).map_err(convert::backend)?,
        payload: serde_json::from_str(&payload).map_err(convert::backend)?,
        state: convert::parse_state(&state)?,
        token: convert::generation_from_i64(token)?,
        lease_expiry: convert::parse_opt_instant(lease_expiry)?,
        attempts: u32::try_from(attempts).map_err(convert::backend)?,
        next_attempt_at: convert::parse_opt_instant(next_attempt_at)?,
        failure_class: convert::parse_class(failure_class.as_deref())?,
        detail,
    })
}

/// Whether every op in `depends_on` has reached terminal success. Scoped to
/// `account`, so a dependency naming another account's op reads as unmet rather than
/// as satisfied by work this account never did.
pub(super) fn dependencies_met(
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
pub(super) fn resource_held_elsewhere(
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
