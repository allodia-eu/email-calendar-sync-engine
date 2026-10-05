//! The host's verbs on an op it did not claim: withdraw it, hurry it, confirm it.
//!
//! Each loads the op, recovers it first if its attempt is dead, and only then decides, so a
//! send that may have been handed over is never withdrawn or sent again as if it had not.

use engine_core::{ids::AccountId, time::UtcDateTime, write::PendingOpId};
use engine_store::{Confirmation, OpRejection, PendingOpState, Result, stays_listed};
use rusqlite::{Connection, Transaction};

use super::{
    recover::recover,
    row::{LoadedOp, load_one_op},
};
use crate::convert;

/// Runs `act` on `op_id` with its dead attempt recovered, committing what it wrote when it
/// took effect (`None`).
fn host_verb(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    now: UtcDateTime,
    act: impl FnOnce(&Transaction<'_>, &LoadedOp) -> Result<Option<OpRejection>>,
) -> Result<Option<OpRejection>> {
    let id = convert::op_id_to_i64(op_id)?;
    let tx = super::begin(conn)?;
    let Some(mut op) = load_one_op(&tx, account.as_str(), id)? else {
        return Ok(Some(OpRejection::Unknown));
    };
    if op.is_dead(now) {
        recover(&tx, &op, now)?;
        op = load_one_op(&tx, account.as_str(), id)?.expect("the op was just loaded");
    }
    let refusal = act(&tx, &op)?;
    // A recovery is committed whatever the verb decided: it is true either way.
    tx.commit().map_err(convert::backend)?;
    Ok(refusal)
}

/// Why withdrawing or hurrying `op` is refused. A send that settled `Failed` is still the
/// host's to act on.
fn refusal(op: &LoadedOp) -> Option<OpRejection> {
    match op.state {
        PendingOpState::Pending => None,
        PendingOpState::InFlight => Some(OpRejection::InFlight),
        PendingOpState::NeedsConfirmation => Some(OpRejection::AwaitingConfirmation),
        PendingOpState::Failed if stays_listed(op.kind, op.state) => None,
        PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::Cancelled => {
            Some(OpRejection::Settled)
        }
    }
}

/// Makes `op` due at once under a new token, with no hand-over, so a late answer from the
/// attempt that held it cannot settle the next one.
fn rearm(tx: &Transaction<'_>, op: &LoadedOp) -> Result<()> {
    tx.execute(
        "UPDATE pending_op
            SET state = 'Pending', token = token + 1, lease_expiry = NULL,
                next_attempt_at = NULL, handed_over = NULL, detail = NULL
          WHERE id = ?1",
        [op.id],
    )
    .map_err(convert::backend)?;
    Ok(())
}

/// Withdraws a queued op, or a send that settled `Failed`, as `Cancelled`.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn cancel(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    now: UtcDateTime,
) -> Result<Option<OpRejection>> {
    let withdraw = |tx: &Transaction<'_>, op: &LoadedOp| {
        if let Some(refused) = refusal(op) {
            return Ok(Some(refused));
        }
        // Bump the token so a worker still holding the old lease cannot resolve it.
        tx.execute(
            "UPDATE pending_op
                SET state = 'Cancelled', token = token + 1, lease_expiry = NULL,
                    next_attempt_at = NULL
              WHERE id = ?1",
            [op.id],
        )
        .map_err(convert::backend)?;
        Ok(None)
    };
    super::durably(conn, |conn| host_verb(conn, account, op_id, now, withdraw))
}

/// Clears a queued op's backoff, or sends a send that settled `Failed` again.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn retry_now(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    now: UtcDateTime,
) -> Result<Option<OpRejection>> {
    host_verb(conn, account, op_id, now, |tx, op| {
        if let Some(refused) = refusal(op) {
            return Ok(Some(refused));
        }
        if op.state == PendingOpState::Failed {
            rearm(tx, op)?;
            return Ok(None);
        }
        // The attempt count stays: one more attempt now, not a fresh bound.
        tx.execute(
            "UPDATE pending_op SET next_attempt_at = NULL WHERE id = ?1",
            [op.id],
        )
        .map_err(convert::backend)?;
        Ok(None)
    })
}

/// Resolves a send awaiting confirmation with what the host learned.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn confirm(
    conn: &mut Connection,
    account: &AccountId,
    op_id: PendingOpId,
    now: UtcDateTime,
    confirmation: Confirmation,
) -> Result<Option<OpRejection>> {
    host_verb(conn, account, op_id, now, |tx, op| {
        match op.state {
            PendingOpState::NeedsConfirmation => {}
            PendingOpState::InFlight => return Ok(Some(OpRejection::InFlight)),
            _ => return Ok(Some(OpRejection::NotAwaitingConfirmation)),
        }
        match confirmation {
            Confirmation::Delivered => {
                tx.execute(
                    "UPDATE pending_op
                        SET state = 'Succeeded', token = token + 1, handed_over = NULL
                      WHERE id = ?1",
                    [op.id],
                )
                .map_err(convert::backend)?;
            }
            Confirmation::NotDelivered => rearm(tx, op)?,
        }
        Ok(None)
    })
}
