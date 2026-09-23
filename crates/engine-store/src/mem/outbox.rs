//! What recording an outcome does to an op, written once for `mark_pending_op` and for
//! start-up recovery.

use engine_core::{time::UtcDateTime, write::PendingOutcome};

use super::{MemStore, OpCell};
use crate::{
    lease::Clock,
    outbox::{MAX_ATTEMPTS, PendingOpState, interrupted_outcome, retry_delay},
};

/// Records `outcome` on `op` at `now`, releasing its lease and counting the attempt.
pub(super) fn record(op: &mut OpCell, outcome: PendingOutcome, now: UtcDateTime) {
    op.lease_expiry = None;
    op.attempts = op.attempts.saturating_add(1);
    match outcome {
        PendingOutcome::Succeeded { .. } => {
            op.state = PendingOpState::Succeeded;
            op.next_attempt_at = None;
            op.failure_class = None;
            op.detail = None;
        }
        PendingOutcome::Failed { class, retry_after } => {
            op.failure_class = Some(class);
            op.detail = None;
            // A class that a plain retry cannot fix settles now; so does one that
            // has used up its attempts. Everything else parks and comes back.
            if class.is_retryable() && op.attempts < MAX_ATTEMPTS {
                op.state = PendingOpState::Pending;
                op.next_attempt_at = now.checked_add(retry_delay(op.attempts, retry_after));
            } else {
                op.state = PendingOpState::Failed;
                op.next_attempt_at = None;
            }
        }
        PendingOutcome::NeedsConfirmation { detail } => {
            op.state = PendingOpState::NeedsConfirmation;
            op.next_attempt_at = None;
            op.detail = Some(detail);
        }
    }
}

impl<C: Clock> MemStore<C> {
    /// [`Store::recover_interrupted_ops`](crate::Store::recover_interrupted_ops).
    pub(super) fn recover_interrupted(&self) -> usize {
        let now = self.clock.now();
        let mut inner = self.lock();
        let mut recovered = 0;
        for op in inner.ops.values_mut() {
            if op.state == PendingOpState::InFlight {
                record(op, interrupted_outcome(op.op.kind), now);
                op.token = op.token.bump();
                recovered += 1;
            }
        }
        recovered
    }
}
