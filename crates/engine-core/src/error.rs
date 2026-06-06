//! Cross-cutting failure classification.
//!
//! Per-type construction errors live next to their types (`IdError`,
//! `TimeError`, …). This module holds the one classification that the sync
//! orchestrator and outbox share: *why* an operation against a provider failed,
//! which decides whether to retry, resync, or surface the failure. It maps the
//! JMAP error taxonomy (RFC 8620 §3.6.2 method errors, §5.3 `SetError`) and the
//! provider classification in `providers.md` onto one enum.

use serde::{Deserialize, Serialize};

/// Why an operation against a provider failed.
///
/// This is the engine-neutral classification; adapters translate their concrete
/// protocol errors into it. Callers must not switch on provider kind — they
/// switch on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FailureClass {
    /// A transient server-side failure (JMAP `serverUnavailable`, a 5xx). Safe
    /// to retry after backoff.
    Retryable,
    /// The provider is throttling or the account is over quota (JMAP
    /// `rateLimit` / `overQuota`). Retry after the supplied delay.
    RateLimited,
    /// Credentials are missing, invalid, or expired. The host must re-auth
    /// before the operation can succeed; plain retry will not help.
    Authentication,
    /// An optimistic-concurrency conflict: the object changed underneath the
    /// write (CalDAV `If-Match` 412, Graph `changeKey` mismatch, JMAP
    /// `stateMismatch`). The caller must refetch and recompute before retrying.
    Conflict,
    /// The operation is not valid in the resource's current state (JMAP
    /// `willDestroy`, a dependency that regressed, `NotRunnable`). Not retryable
    /// as-is.
    InvalidState,
    /// The sync cursor cannot produce a delta and the scope must be fully
    /// resynced (JMAP `cannotCalculateChanges` / `serverPartialFail`, an IMAP
    /// `UIDVALIDITY` reset).
    NeedsResync,
    /// A permanent failure: the request will never succeed unchanged (JMAP
    /// `forbidden`, `invalidProperties`, `notFound`, `singleton`). Do not retry.
    Permanent,
}

impl FailureClass {
    /// Returns `true` if the operation may be retried unchanged after a backoff
    /// delay. Conflicts and resync-required failures are deliberately excluded:
    /// they need recomputation, not plain retry.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, FailureClass::Retryable | FailureClass::RateLimited)
    }

    /// Returns `true` if the failure means the affected sync scope must be fully
    /// resynced rather than retried or recomputed in place.
    #[must_use]
    pub fn requires_resync(self) -> bool {
        matches!(self, FailureClass::NeedsResync)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classes_are_exactly_transient_and_rate_limited() {
        assert!(FailureClass::Retryable.is_retryable());
        assert!(FailureClass::RateLimited.is_retryable());
        for class in [
            FailureClass::Authentication,
            FailureClass::Conflict,
            FailureClass::InvalidState,
            FailureClass::NeedsResync,
            FailureClass::Permanent,
        ] {
            assert!(
                !class.is_retryable(),
                "{class:?} must not be plain-retryable"
            );
        }
    }

    #[test]
    fn only_needs_resync_requires_resync() {
        assert!(FailureClass::NeedsResync.requires_resync());
        for class in [
            FailureClass::Retryable,
            FailureClass::RateLimited,
            FailureClass::Authentication,
            FailureClass::Conflict,
            FailureClass::InvalidState,
            FailureClass::Permanent,
        ] {
            assert!(!class.requires_resync());
        }
    }

    #[test]
    fn classification_roundtrips_through_json() {
        let class = FailureClass::Conflict;
        let json = serde_json::to_string(&class).unwrap();
        assert_eq!(json, "\"Conflict\"");
        assert_eq!(serde_json::from_str::<FailureClass>(&json).unwrap(), class);
    }
}
