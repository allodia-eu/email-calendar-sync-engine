//! The outbox op state machine and the claimed-op handle.
//!
//! Pending ops are durable before any side effect and claimed with the same
//! fencing discipline as scopes (`store-and-sync.md`). This module fixes the
//! lifecycle state a store tracks per op and the handle a worker resolves under.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    time::UtcDateTime,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, ResourceKey},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lease::OpLease;

/// The lifecycle state of a durable pending operation.
///
/// Claim moves a runnable op to `InFlight` under an [`OpLease`]; an ambiguous
/// outcome parks it in `NeedsConfirmation` rather than blind-retrying. A
/// dependent op only becomes runnable once its dependencies reach `Succeeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PendingOpState {
    /// Durable and runnable, not yet claimed.
    Pending,
    /// Leased by a worker; the provider side effect is in flight.
    InFlight,
    /// The outcome is ambiguous and awaits sync, `Message-ID` lookup, or explicit
    /// host/user confirmation.
    NeedsConfirmation,
    /// Terminal: the op succeeded and resolved to a provider key.
    Succeeded,
    /// Terminal: the op failed, and the outbox will not attempt it again. A
    /// retryable failure does **not** land here until its attempts run out; it
    /// parks back in `Pending` with a `next_attempt_at`.
    Failed,
    /// Terminal: the host withdrew the op before it reached the provider.
    ///
    /// Distinct from `Failed` because nothing went wrong: a user deleted a queued
    /// message. A host that rendered a withdrawal as a failure would be reporting
    /// its own action back as an error.
    Cancelled,
}

impl PendingOpState {
    /// Returns `true` if this is a terminal state with no further transitions.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Returns `true` if a dependent op may now run (this dependency reached
    /// terminal success).
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// A claimed, runnable pending op handed to a worker.
///
/// Carries the op envelope, its store id, and the [`OpLease`] its resolution must
/// present to `mark_pending_op`; the store rejects a stale token. Does not implement
/// `Eq` for the reason [`PendingOp`] does not: payloads are arbitrary JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct LeasedPendingOp {
    /// The store-assigned id of the op.
    pub id: PendingOpId,
    /// The op envelope (idempotency key, dependencies, resource key, payload).
    pub op: PendingOp,
    /// The lease under which the outcome must be reported.
    pub lease: OpLease,
}

impl LeasedPendingOp {
    /// Bundles a claimed op with its id and lease.
    #[must_use]
    pub fn new(id: PendingOpId, op: PendingOp, lease: OpLease) -> Self {
        Self { id, op, lease }
    }
}

/// The result of claiming one **named** op (`Store::claim_pending_op`).
///
/// The batch claim answers "what may run now"; this answers "may *this* run now",
/// which is the question an inline driver resolving the op it just enqueued has.
#[derive(Debug, Clone, PartialEq)]
pub enum PendingOpClaim {
    /// Leased under a fresh fencing token, ready to resolve.
    Leased(Box<LeasedPendingOp>),
    /// Not leased, and which condition refused it.
    Refused(ClaimRejection),
}

/// Why a targeted claim did not lease the op it named.
///
/// The distinction is what a caller needs to act: a [`Busy`](ClaimRejection::Busy)
/// resource is a wait, and every other refusal is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaimRejection {
    /// No op with that id in this account's outbox.
    Unknown,
    /// An outcome is already recorded for it: terminal, or parked in
    /// [`NeedsConfirmation`](PendingOpState::NeedsConfirmation).
    Settled,
    /// A `depends_on` op has not reached terminal success.
    DependencyUnmet,
    /// A live lease holds this op, or another op holding its `resource_key`.
    Busy,
    /// A retryable failure parked the op until its `next_attempt_at`, which has not
    /// arrived. Not [`Settled`](ClaimRejection::Settled): it will run, just not yet.
    Backoff,
}

/// How many attempts a retryable op gets before the outbox stops trying.
///
/// A bound has to exist: without one, a server that keeps answering `503` turns a queued
/// write into a permanent background round trip the user never asked for and cannot see
/// the end of. Eight attempts on [`retry_delay`]'s schedule spans a little over an hour,
/// which covers a restart, a flaky link and a short provider outage without pretending an
/// hours-old failure is still transient.
pub const MAX_ATTEMPTS: u32 = 8;

/// The first retry delay; each further attempt doubles it up to [`RETRY_CAP`].
const RETRY_BASE: Duration = Duration::from_secs(30);

/// The ceiling on a *derived* delay. A provider's own `retry_after` is obeyed past it:
/// a server saying "come back in an hour" is an instruction, not a hint to average down.
const RETRY_CAP: Duration = Duration::from_mins(30);

/// When a retryable failure may be attempted again.
///
/// `attempts` is the number already made, so the first failure passes 1. `hint` is the
/// provider's `retry_after` and wins outright when present (`RateLimited` carries one, and
/// obeying it is what stops a retry storm making the throttle worse); otherwise the delay
/// doubles from a 30-second base, capped at 30 minutes.
#[must_use]
pub fn retry_delay(attempts: u32, hint: Option<engine_core::time::Duration>) -> Duration {
    if let Some(hint) = hint {
        let seconds = hint
            .days()
            .saturating_mul(86_400)
            .saturating_add(hint.seconds());
        return Duration::new(seconds, hint.nanoseconds());
    }
    let doublings = attempts.saturating_sub(1).min(16);
    RETRY_BASE.saturating_mul(1_u32 << doublings).min(RETRY_CAP)
}

/// One row of an account's outbox, as a host or a drainer reads it back.
///
/// The read counterpart of [`LeasedPendingOp`]: everything needed to decide whether an op
/// may run, and to tell a user what is queued and why it has not gone, without claiming
/// anything. Does not implement `Eq`, for the reason [`PendingOp`] does not.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingOpRow {
    /// The store-assigned id.
    pub id: PendingOpId,
    /// Which write [`payload`](Self::payload) describes.
    ///
    /// `None` for a row enqueued before the store recorded a kind. Such a row cannot be
    /// dispatched (nothing says which request type its payload is), so it is never
    /// claimed; it is listed so a host can show it and cancel it.
    pub kind: Option<PendingOpKind>,
    /// Makes enqueuing idempotent.
    pub idempotency_key: IdempotencyKey,
    /// The resource this op serializes on.
    pub resource_key: ResourceKey,
    /// The operation description, interpreted by the outbox/provider layer.
    pub payload: Value,
    /// Where the op is in its lifecycle.
    pub state: PendingOpState,
    /// How many provider attempts have been made.
    pub attempts: u32,
    /// The earliest time a parked retry may be claimed, if it is waiting on one.
    pub next_attempt_at: Option<UtcDateTime>,
    /// How the last attempt failed, if one did. Survives a park back into
    /// [`Pending`](PendingOpState::Pending), so a host can say *why* something is still
    /// queued rather than only that it is.
    pub failure_class: Option<FailureClass>,
    /// Provider detail for the last failure or ambiguity.
    pub detail: Option<String>,
}

/// Why a host's request to act on a queued op did not take effect.
///
/// Shared by the two things a host can do to an op it did not claim: withdraw it
/// (`Store::cancel_pending_op`) and hurry it (`Store::retry_pending_op_now`). The
/// conditions are the same either way, because both are a host reaching for an op some
/// other worker may already own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpRejection {
    /// No op with that id in this account's outbox.
    Unknown,
    /// The op has already settled; there is nothing left to act on.
    Settled,
    /// A live lease holds the op: the provider side effect may be happening right
    /// now, so withdrawing it would claim to have stopped something that already went
    /// out, and hurrying it would ask for a second attempt while the first is running.
    /// The caller retries once the lease lapses.
    InFlight,
    /// The op is parked in [`NeedsConfirmation`](PendingOpState::NeedsConfirmation):
    /// it may already have been delivered, so it is resolved by confirmation, never by
    /// withdrawal and never by another attempt.
    AwaitingConfirmation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_success_classification() {
        assert!(PendingOpState::Succeeded.is_terminal());
        assert!(PendingOpState::Failed.is_terminal());
        assert!(PendingOpState::Cancelled.is_terminal());
        assert!(!PendingOpState::Pending.is_terminal());
        assert!(!PendingOpState::InFlight.is_terminal());
        assert!(!PendingOpState::NeedsConfirmation.is_terminal());

        assert!(PendingOpState::Succeeded.is_success());
        for state in [
            PendingOpState::Pending,
            PendingOpState::InFlight,
            PendingOpState::NeedsConfirmation,
            PendingOpState::Failed,
            PendingOpState::Cancelled,
        ] {
            assert!(!state.is_success());
        }
    }

    #[test]
    fn a_derived_retry_delay_doubles_and_then_stops_growing() {
        assert_eq!(retry_delay(1, None), Duration::from_secs(30));
        assert_eq!(retry_delay(2, None), Duration::from_mins(1));
        assert_eq!(retry_delay(3, None), Duration::from_mins(2));
        // Capped, and it stays capped however many attempts are passed: the
        // doubling must not overflow the shift on a long-lived op.
        assert_eq!(retry_delay(MAX_ATTEMPTS, None), RETRY_CAP);
        assert_eq!(retry_delay(u32::MAX, None), RETRY_CAP);
    }

    #[test]
    fn a_provider_retry_after_wins_over_the_derived_delay() {
        // Shorter than the derived delay for that attempt, and longer than the cap:
        // both are the server's instruction and neither is second-guessed.
        let short = "PT5S".parse().unwrap();
        assert_eq!(retry_delay(4, Some(short)), Duration::from_secs(5));
        let long = "PT2H".parse().unwrap();
        assert_eq!(retry_delay(1, Some(long)), Duration::from_hours(2));
    }

    #[test]
    fn state_roundtrips_through_json() {
        let state = PendingOpState::NeedsConfirmation;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"NeedsConfirmation\"");
        assert_eq!(
            serde_json::from_str::<PendingOpState>(&json).unwrap(),
            state
        );
    }
}
