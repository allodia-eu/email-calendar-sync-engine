//! The outbox op state machine and the claimed-op handle.
//!
//! Pending ops are durable before any side effect and claimed with the same
//! fencing discipline as scopes (`store-and-sync.md`). This module fixes the
//! lifecycle state a store tracks per op and the handle a worker resolves under.

use engine_core::write::{PendingOp, PendingOpId};
use serde::{Deserialize, Serialize};

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
    /// Terminal: the op failed.
    Failed,
}

impl PendingOpState {
    /// Returns `true` if this is a terminal state with no further transitions.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_and_success_classification() {
        assert!(PendingOpState::Succeeded.is_terminal());
        assert!(PendingOpState::Failed.is_terminal());
        assert!(!PendingOpState::Pending.is_terminal());
        assert!(!PendingOpState::InFlight.is_terminal());
        assert!(!PendingOpState::NeedsConfirmation.is_terminal());

        assert!(PendingOpState::Succeeded.is_success());
        for state in [
            PendingOpState::Pending,
            PendingOpState::InFlight,
            PendingOpState::NeedsConfirmation,
            PendingOpState::Failed,
        ] {
            assert!(!state.is_success());
        }
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
