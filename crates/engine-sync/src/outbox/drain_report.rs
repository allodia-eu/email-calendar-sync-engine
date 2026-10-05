//! What a drain pass reports: one entry per op it attempted, and what became of each.

use engine_core::{
    error::FailureClass,
    ids::ProviderKey,
    write::{PendingOpId, PendingOpKind},
};

/// What one drain pass did, one entry per op it attempted.
///
/// A pass that attempted nothing is not an error: an empty outbox, ops still waiting out a
/// backoff, and a queue of kinds this pass cannot dispatch all reach it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DrainReport {
    /// The ops this pass attempted, in the order it took them.
    pub attempted: Vec<DrainedOp>,
    /// Ops left for a later pass: not yet due, serialized behind another op, or of a kind
    /// this pass does not dispatch. None of them was leased.
    pub deferred: usize,
}

impl DrainReport {
    /// How many provider calls succeeded, a delivered-but-unfiled send included: the
    /// message went out either way, which is the fact a caller acts on.
    #[must_use]
    pub fn delivered(&self) -> usize {
        self.attempted
            .iter()
            .filter(|op| {
                matches!(
                    op.outcome,
                    DrainOutcome::Succeeded | DrainOutcome::SentNotFiled { .. }
                )
            })
            .count()
    }

    /// Whether this pass changed nothing, so a caller can skip a refresh.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.attempted.is_empty()
    }
}

/// One op a drain pass attempted, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainedOp {
    /// The durable op.
    pub id: PendingOpId,
    /// Which write it was.
    pub kind: PendingOpKind,
    /// What the provider call did.
    pub outcome: DrainOutcome,
    /// The key the provider resolved the write to, when it succeeded and there is one.
    ///
    /// **This is the only place a caller learns what a queued write became.** An op that
    /// parked while offline succeeds with nobody watching, and the account then holds an
    /// object the caller has never seen a key for. A draft is what makes it matter: the
    /// next save has to name the copy it supersedes, or it stores a second one beside it,
    /// and on three of the four adapters the key is not the one that went in
    /// (`providers.md`).
    pub provider_key: Option<ProviderKey>,
}

/// The outcome of one drained op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// The provider call succeeded and the op settled.
    Succeeded,
    /// A submission was **delivered** and the sender's copy was not filed.
    ///
    /// Its own variant because the two facts have to travel together: folding it into
    /// [`Succeeded`](DrainOutcome::Succeeded) loses the copy in silence, and folding it
    /// into a failure invites re-sending mail the recipients already have. The op is
    /// settled either way — the mail has gone.
    SentNotFiled {
        /// Why filing failed: a class and protocol detail, never draft content.
        detail: String,
    },
    /// The call failed retryably, so the op is queued again for a later pass.
    Parked {
        /// How it failed.
        class: FailureClass,
        /// How many attempts it has now had.
        attempts: u32,
    },
    /// The call failed in a way no retry fixes, or the op ran out of attempts. It will
    /// not be attempted again.
    Failed {
        /// How it failed.
        class: FailureClass,
    },
    /// A send whose outcome is genuinely ambiguous: parked for confirmation and **never**
    /// retried, so the outbox cannot double-send.
    AwaitingConfirmation {
        /// The provider's description of the ambiguity.
        detail: String,
    },
    /// The stored payload could not be read as the request its kind names, so the op
    /// never reached the provider and is settled.
    ///
    /// Distinct from [`Failed`](DrainOutcome::Failed), which is the provider refusing:
    /// nothing was asked of it. A payload written by a build this one cannot read reaches
    /// here, and no number of retries changes that, so the op settles rather than
    /// blocking the pass behind it for ever.
    Undecodable {
        /// What could not be decoded.
        detail: String,
    },
}
