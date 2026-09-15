//! The lease-free outbox reads: one op's state, and the account's queue.

use engine_core::{ids::AccountId, write::PendingOpId};
use engine_store::{PendingOpRow, PendingOpState, Result};
use rusqlite::{Connection, OptionalExtension};

use super::row::{OP_COLUMNS, parse_op_row, read_op_row};
use crate::convert;

/// The current lifecycle state of an op, or `None` if unknown.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
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

/// Every outbox row for `account` that has not settled, in enqueue order.
///
/// Filtered in SQL rather than after the load: a settled row is kept for ever as the
/// idempotency record, so an account's terminal rows outnumber its outstanding ones by
/// more every write, and a host polls this to draw its outbox.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn list_pending_ops(
    conn: &Connection,
    account: &AccountId,
) -> Result<Vec<PendingOpRow>> {
    let sql = format!(
        "SELECT {OP_COLUMNS} FROM pending_op
          WHERE account = ?1 AND state IN ('Pending', 'InFlight', 'NeedsConfirmation')
          ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql).map_err(convert::backend)?;
    let raws = stmt
        .query_map([account.as_str()], read_op_row)
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    raws.into_iter()
        .map(|raw| {
            let op = parse_op_row(raw)?;
            Ok(PendingOpRow {
                id: convert::op_id_from_i64(op.id)?,
                kind: op.kind,
                idempotency_key: engine_core::write::IdempotencyKey::new(op.idempotency_key)
                    .map_err(convert::backend)?,
                resource_key: engine_core::write::ResourceKey::new(op.resource_key)
                    .map_err(convert::backend)?,
                payload: op.payload,
                state: op.state,
                attempts: op.attempts,
                next_attempt_at: op.next_attempt_at,
                failure_class: op.failure_class,
                detail: op.detail,
            })
        })
        .collect()
}
