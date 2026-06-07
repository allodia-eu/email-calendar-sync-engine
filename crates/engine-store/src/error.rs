//! Store error taxonomy.
//!
//! Store errors map onto the provider [`FailureClass`] taxonomy
//! (`providers.md`, `store-and-sync.md`): the orchestrator switches on this, not
//! on a concrete backend error.

use engine_core::error::FailureClass;

/// The result type for store operations.
pub type Result<T> = core::result::Result<T, StoreError>;

/// Why a store operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The lease token was superseded by a newer claim. Not retryable as-is:
    /// re-claim, recompute, and reapply.
    #[error("stale lease: fencing token superseded by a newer claim")]
    StaleLease,
    /// A live, unexpired lease already exists for the scope or op. Retryable
    /// after backoff.
    #[error("scope or op is already leased")]
    ScopeHeld,
    /// An optimistic-concurrency conflict surfaced from the store (e.g. a
    /// snapshot racing a concurrent delta). Recompute before retrying.
    #[error("optimistic write conflict")]
    Conflict,
    /// An op was asked to resolve but its dependencies regressed out of a
    /// runnable state.
    #[error("pending op is not runnable: dependencies regressed")]
    NotRunnable,
    /// A backend failure (I/O, serialization, SQL). The concrete error stays in
    /// the backend; this carries a redacted message.
    #[error("store backend error: {0}")]
    Backend(String),
}

impl StoreError {
    /// The provider failure class this store error maps to, so retry/resync
    /// decisions are uniform across store and provider operations.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::StaleLease | Self::Conflict => FailureClass::Conflict,
            Self::ScopeHeld | Self::Backend(_) => FailureClass::Retryable,
            Self::NotRunnable => FailureClass::InvalidState,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_provider_failure_classes() {
        assert_eq!(
            StoreError::StaleLease.failure_class(),
            FailureClass::Conflict
        );
        assert_eq!(StoreError::Conflict.failure_class(), FailureClass::Conflict);
        assert_eq!(
            StoreError::ScopeHeld.failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            StoreError::NotRunnable.failure_class(),
            FailureClass::InvalidState
        );
        assert_eq!(
            StoreError::Backend("disk full".into()).failure_class(),
            FailureClass::Retryable
        );
    }

    #[test]
    fn stale_lease_is_not_plain_retryable() {
        // A stale lease must be recomputed, not retried unchanged.
        assert!(!StoreError::StaleLease.failure_class().is_retryable());
    }
}
