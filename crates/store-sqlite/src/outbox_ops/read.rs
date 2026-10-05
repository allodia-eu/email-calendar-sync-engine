//! The lease-free outbox reads: one op's state, and the account's queue.
//!
//! Both show a dead attempt as its recovery will leave it (`LoadedOp::view`), so a host never
//! sees a send held "in flight" by a worker that is gone.

use engine_core::{ids::AccountId, time::UtcDateTime, write::PendingOpId};
use engine_store::{PendingOpRow, PendingOpState, Result, stays_listed};
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
    now: UtcDateTime,
) -> Result<Option<PendingOpState>> {
    let id = convert::op_id_to_i64(op_id)?;
    let sql = format!("SELECT {OP_COLUMNS} FROM pending_op WHERE id = ?1");
    let raw = conn
        .query_row(&sql, [id], read_op_row)
        .optional()
        .map_err(convert::backend)?;
    match raw {
        Some(raw) => Ok(Some(parse_op_row(raw)?.view(now)?.state)),
        None => Ok(None),
    }
}

/// Every outbox row for `account` that is still the host's concern, in enqueue order: what
/// has not settled, and a send that settled `Failed` (`engine_store::stays_listed`).
///
/// Filtered in SQL rather than after the load: a settled row is kept for ever as the
/// idempotency record, so an account's terminal rows outnumber its outstanding ones by
/// more every write, and a host polls this to draw its outbox. The `WHERE` clause is
/// `stays_listed` over the stored state; only an `InFlight` row can be shown in another
/// state, and never in one the clause would drop.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn list_pending_ops(
    conn: &Connection,
    account: &AccountId,
    now: UtcDateTime,
) -> Result<Vec<PendingOpRow>> {
    let sql = format!(
        "SELECT {OP_COLUMNS} FROM pending_op
          WHERE account = ?1
            AND (state IN ('Pending', 'InFlight', 'NeedsConfirmation')
                 OR (state = 'Failed' AND kind = 'MailSubmit'))
          ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql).map_err(convert::backend)?;
    let stored = stmt
        .query_map([account.as_str()], read_op_row)
        .map_err(convert::backend)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(convert::backend)?;

    let mut rows = Vec::with_capacity(stored.len());
    for raw in stored {
        let op = parse_op_row(raw)?;
        let view = op.view(now)?;
        if !stays_listed(op.kind, view.state) {
            continue;
        }
        rows.push(PendingOpRow {
            id: convert::op_id_from_i64(op.id)?,
            kind: op.kind,
            idempotency_key: engine_core::write::IdempotencyKey::new(op.idempotency_key)
                .map_err(convert::backend)?,
            resource_key: engine_core::write::ResourceKey::new(op.resource_key)
                .map_err(convert::backend)?,
            payload: op.payload,
            state: view.state,
            attempts: view.attempts,
            next_attempt_at: view.next_attempt_at,
            failure_class: view.failure_class,
            detail: view.detail,
        });
    }
    Ok(rows)
}
