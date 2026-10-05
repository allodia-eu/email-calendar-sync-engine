//! What recording an outcome does to an op, decided once for every backend.
//!
//! A store records an outcome from three places: a worker reporting its own attempt
//! (`mark_pending_op`), the recovery of an attempt whose worker is gone, and the queue read,
//! which shows a dead attempt as its recovery will leave it. All three go through
//! [`record_outcome`], so the in-memory store, SQLite and a read of either cannot disagree about
//! where an op lands.

use engine_core::{
    error::FailureClass,
    time::UtcDateTime,
    write::{PendingOpKind, PendingOutcome},
};

use crate::{
    error::{Result, StoreError},
    outbox::{MAX_ATTEMPTS, PendingOpState, retry_delay},
};

/// Where an op stands once an outcome is recorded on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// The lifecycle state it moves to.
    pub state: PendingOpState,
    /// The attempt count, this attempt included.
    pub attempts: u32,
    /// When a parked retry becomes due, if it parked.
    pub next_attempt_at: Option<UtcDateTime>,
    /// How the attempt failed, if it did.
    pub failure_class: Option<FailureClass>,
    /// The detail an ambiguous outcome carries.
    pub detail: Option<String>,
    /// Whether the attempt's hand-over record stays on the op.
    ///
    /// Only an ambiguous outcome keeps it: the record is what lets the attempt that handed the
    /// message over still report what became of it. A definitive answer, refusal or
    /// acceptance, ends the question, and the next attempt starts with none.
    pub keeps_hand_over: bool,
}

/// Whether a retryable failure of `kind` settles once [`MAX_ATTEMPTS`] are spent.
///
/// A send does not. It is the user's own work, and the outbox is the only copy the engine
/// keeps: settling it on a count turns a server that was down for an afternoon into a message
/// that is quietly not going to be sent. Waiting costs nothing, because an attempt happens
/// only when the host drains the outbox, and the row stays listed with its attempts and its
/// last failure, so a host can say why it has not gone and the user can withdraw it.
#[must_use]
pub fn settles_on_attempts(kind: Option<PendingOpKind>) -> bool {
    kind != Some(PendingOpKind::MailSubmit)
}

/// Records `outcome` on an op of `kind` that had made `attempts_before` attempts, at `now`.
///
/// A retryable failure parks the op in `Pending` with a backoff from [`retry_delay`]; every
/// other failure, and a retryable one that has spent its attempts where
/// [`settles_on_attempts`] says it can, settles it as `Failed`.
///
/// # Errors
///
/// [`StoreError::Backend`] if the retry instant overflows.
pub fn record_outcome(
    kind: Option<PendingOpKind>,
    attempts_before: u32,
    outcome: &PendingOutcome,
    now: UtcDateTime,
) -> Result<Recorded> {
    let attempts = attempts_before.saturating_add(1);
    let recorded = match outcome {
        PendingOutcome::Succeeded { .. } => Recorded {
            state: PendingOpState::Succeeded,
            attempts,
            next_attempt_at: None,
            failure_class: None,
            detail: None,
            keeps_hand_over: false,
        },
        PendingOutcome::Failed { class, retry_after } => {
            let parks =
                class.is_retryable() && (!settles_on_attempts(kind) || attempts < MAX_ATTEMPTS);
            let next_attempt_at = if parks {
                let due = now
                    .checked_add(retry_delay(attempts, *retry_after))
                    .ok_or_else(|| StoreError::Backend("retry delay overflow".to_owned()))?;
                Some(due)
            } else {
                None
            };
            Recorded {
                state: if parks {
                    PendingOpState::Pending
                } else {
                    PendingOpState::Failed
                },
                attempts,
                next_attempt_at,
                failure_class: Some(*class),
                detail: None,
                keeps_hand_over: false,
            }
        }
        PendingOutcome::NeedsConfirmation { detail } => Recorded {
            state: PendingOpState::NeedsConfirmation,
            attempts,
            next_attempt_at: None,
            failure_class: None,
            detail: Some(detail.clone()),
            keeps_hand_over: true,
        },
    };
    Ok(recorded)
}

/// What an attempt whose worker is gone is recorded as.
///
/// Whatever its own failure would be, and for a send that turns on one fact: whether the
/// attempt had recorded that it was handing the message over (`Store::record_hand_over`).
///
/// - A send that had **not** is a retryable failure, due at once. Nothing that could deliver it had
///   been written, so closing the app while it connected costs a retry and nothing else.
/// - A send that **had** may be in front of its recipients. It awaits confirmation, which no claim
///   takes, exactly as a lost acknowledgement to the end of the message does.
/// - Every other write is the retryable failure a timeout is: it parks, comes back after the
///   backoff, and settles once its attempts run out, so an op that takes the process down every
///   time it runs stops eventually.
#[must_use]
pub fn interrupted_outcome(kind: PendingOpKind, handed_over: bool) -> PendingOutcome {
    match kind {
        PendingOpKind::MailSubmit if handed_over => PendingOutcome::NeedsConfirmation {
            detail: "the attempt ended after the message was handed to the server, so whether it \
                     was delivered is unknown"
                .to_owned(),
        },
        PendingOpKind::MailSubmit => PendingOutcome::Failed {
            class: FailureClass::Retryable,
            retry_after: Some(engine_core::time::Duration::ZERO),
        },
        PendingOpKind::MailEdit
        | PendingOpKind::MailReport
        | PendingOpKind::MailDraftPut
        | PendingOpKind::MailDraftDelete
        | PendingOpKind::MailboxEdit
        | PendingOpKind::CalendarCreate
        | PendingOpKind::CalendarPatch
        | PendingOpKind::CalendarDocument
        | PendingOpKind::CalendarRsvp
        | PendingOpKind::CalendarDelete
        | PendingOpKind::ContactCreate
        | PendingOpKind::ContactPatch
        | PendingOpKind::ContactDelete => PendingOutcome::Failed {
            class: FailureClass::Retryable,
            retry_after: None,
        },
    }
}

/// Whether the queue read lists an op of `kind` in `state`.
///
/// Every op that has not settled, and a send that settled `Failed`: that one is a message the
/// user wrote and that has not gone, so it stays in front of them until they send it again
/// (`retry_pending_op_now`) or withdraw it (`cancel_pending_op`).
#[must_use]
pub fn stays_listed(kind: Option<PendingOpKind>, state: PendingOpState) -> bool {
    !state.is_terminal()
        || (state == PendingOpState::Failed && kind == Some(PendingOpKind::MailSubmit))
}

#[cfg(test)]
#[path = "settle_tests.rs"]
mod tests;
