//! Recording an outcome on an op, and recovering an attempt whose worker is gone.
//!
//! Both decide through `engine_store::record_outcome`, the rule every backend shares, so this
//! file only says how the result lands in a row.

use engine_core::{time::UtcDateTime, write::PendingOutcome};
use engine_store::{Recorded, Result, interrupted_outcome, record_outcome};
use rusqlite::{Connection, Transaction};

use super::row::{LoadedOp, load_in_flight_ops};
use crate::convert;

impl LoadedOp {
    /// Where `outcome` would leave this op, without writing anything.
    pub(super) fn recorded(&self, outcome: &PendingOutcome, now: UtcDateTime) -> Result<Recorded> {
        record_outcome(self.kind, self.attempts, outcome, now)
    }

    /// What recovering this op's attempt records, or `None` for a row with no kind, which no
    /// claim leases and so no attempt can have left in flight.
    pub(super) fn interrupted(&self) -> Option<PendingOutcome> {
        Some(interrupted_outcome(self.kind?, self.handed_over.is_some()))
    }

    /// This op as a read shows it: a dead attempt as its recovery will leave it, so a host
    /// never sees a send stuck "in flight" under a worker that is gone. The recovery itself is
    /// durable only on a write path; a read runs on a reader connection.
    pub(super) fn view(&self, now: UtcDateTime) -> Result<Recorded> {
        if self.is_dead(now)
            && let Some(outcome) = self.interrupted()
        {
            return self.recorded(&outcome, now);
        }
        Ok(Recorded {
            state: self.state,
            attempts: self.attempts,
            next_attempt_at: self.next_attempt_at,
            failure_class: self.failure_class,
            detail: self.detail.clone(),
            keeps_hand_over: self.handed_over.is_some(),
        })
    }
}

/// Records `outcome` on `op`: releases the lease, counts the attempt, parks, settles or awaits
/// confirmation as the shared rule says, and drops the hand-over unless the outcome is
/// ambiguous.
pub(super) fn record(
    tx: &Transaction<'_>,
    op: &LoadedOp,
    now: UtcDateTime,
    outcome: &PendingOutcome,
) -> Result<()> {
    let recorded = op.recorded(outcome, now)?;
    tx.execute(
        "UPDATE pending_op
            SET state = ?1, lease_expiry = NULL, attempts = ?2, next_attempt_at = ?3,
                failure_class = ?4, detail = ?5,
                handed_over = CASE WHEN ?6 THEN handed_over ELSE NULL END
          WHERE id = ?7",
        (
            convert::state_to_text(recorded.state),
            i64::from(recorded.attempts),
            recorded.next_attempt_at.map(convert::instant_to_text),
            recorded.failure_class.map(convert::class_to_text),
            recorded.detail,
            recorded.keeps_hand_over,
            op.id,
        ),
    )
    .map_err(convert::backend)?;
    Ok(())
}

/// Recovers `op`'s attempt and fences its worker out by bumping the token. Returns whether it
/// did: a row with no kind is left as it is.
pub(super) fn recover(tx: &Transaction<'_>, op: &LoadedOp, now: UtcDateTime) -> Result<bool> {
    let Some(outcome) = op.interrupted() else {
        return Ok(false);
    };
    tx.execute(
        "UPDATE pending_op SET token = token + 1 WHERE id = ?1",
        [op.id],
    )
    .map_err(convert::backend)?;
    record(tx, op, now, &outcome)?;
    Ok(true)
}

/// Recovers every attempt of `account` whose lease has lapsed. Every path that decides what
/// to do with an op runs this first, inside its own transaction, so none of them can meet a
/// dead attempt and treat it as live or as runnable.
pub(super) fn recover_dead(tx: &Transaction<'_>, account: &str, now: UtcDateTime) -> Result<()> {
    for op in load_in_flight_ops(tx, Some(account))? {
        if op.is_dead(now) {
            recover(tx, &op, now)?;
        }
    }
    Ok(())
}

/// Recovers every op left `InFlight`, live lease or not, in one transaction
/// (`Store::recover_interrupted_ops`). Returns how many it recovered.
///
/// # Errors
///
/// Returns [`engine_store::StoreError::Backend`] on a backend failure.
pub(crate) fn recover_interrupted(conn: &mut Connection, now: UtcDateTime) -> Result<usize> {
    let tx = super::begin(conn)?;
    let mut recovered = 0;
    for op in load_in_flight_ops(&tx, None)? {
        if recover(&tx, &op, now)? {
            recovered += 1;
        }
    }
    tx.commit().map_err(convert::backend)?;
    Ok(recovered)
}
