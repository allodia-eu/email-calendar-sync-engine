//! Provider failure classification.
//!
//! An adapter translates its concrete protocol error (a JMAP method error or
//! `SetError`, an HTTP status, a transport failure) into a [`ProviderError`]
//! carrying an engine-neutral [`FailureClass`]. Callers switch on the class — the
//! same taxonomy the store and outbox use (`providers.md`,
//! `store-and-sync.md`) — and **never** on the provider kind.
//!
//! The classification itself lives in `engine-core` ([`FailureClass`]); this type
//! pairs it with human context, an optional retry delay (for rate limits), and an
//! optional wrapped source so the original protocol error stays inspectable.

use core::fmt;

use engine_core::error::FailureClass;
use engine_core::time::Duration;

/// A boxed underlying error, kept so the original protocol/transport failure is
/// still reachable through [`std::error::Error::source`].
type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// A classified provider failure.
///
/// Construct one through a class-named constructor ([`ProviderError::authentication`],
/// [`ProviderError::rate_limited`], …) so the [`FailureClass`] and the message stay
/// in sync. [`ProviderError::class`] is what callers branch on.
#[derive(Debug)]
pub struct ProviderError {
    class: FailureClass,
    detail: String,
    retry_after: Option<Duration>,
    source: Option<BoxError>,
}

impl ProviderError {
    /// Creates an error with an explicit class and message.
    #[must_use]
    pub fn new(class: FailureClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
            retry_after: None,
            source: None,
        }
    }

    /// A transient server-side failure, safe to retry after backoff
    /// ([`FailureClass::Retryable`]).
    #[must_use]
    pub fn retryable(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::Retryable, detail)
    }

    /// The provider is throttling or the account is over quota
    /// ([`FailureClass::RateLimited`]); retry after `retry_after`.
    #[must_use]
    pub fn rate_limited(detail: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self {
            retry_after,
            ..Self::new(FailureClass::RateLimited, detail)
        }
    }

    /// Credentials are missing, invalid, or expired ([`FailureClass::Authentication`]);
    /// the host must re-auth before the call can succeed.
    #[must_use]
    pub fn authentication(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::Authentication, detail)
    }

    /// An optimistic-concurrency conflict ([`FailureClass::Conflict`]); refetch and
    /// recompute before retrying.
    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::Conflict, detail)
    }

    /// The operation is invalid in the resource's current state
    /// ([`FailureClass::InvalidState`]); not retryable as-is.
    #[must_use]
    pub fn invalid_state(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::InvalidState, detail)
    }

    /// The cursor cannot produce a delta and the scope must be fully resynced
    /// ([`FailureClass::NeedsResync`] — JMAP `cannotCalculateChanges`).
    #[must_use]
    pub fn needs_resync(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::NeedsResync, detail)
    }

    /// A permanent failure that will never succeed unchanged
    /// ([`FailureClass::Permanent`]).
    #[must_use]
    pub fn permanent(detail: impl Into<String>) -> Self {
        Self::new(FailureClass::Permanent, detail)
    }

    /// Attaches the underlying protocol/transport error as the [`source`](std::error::Error::source).
    #[must_use]
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// The engine-neutral failure class callers branch on.
    #[must_use]
    pub fn class(&self) -> FailureClass {
        self.class
    }

    /// The human-facing detail message.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The delay to wait before retrying, when the provider supplied one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Whether the operation may be retried unchanged after a backoff.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.class.is_retryable()
    }

    /// Whether the failure means the scope must be fully resynced.
    #[must_use]
    pub fn requires_resync(&self) -> bool {
        self.class.requires_resync()
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} provider error: {}", self.class, self.detail)
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// The result type provider methods return.
pub type ProviderResult<T> = Result<T, ProviderError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_named_constructors_set_the_right_class() {
        assert_eq!(
            ProviderError::authentication("no token").class(),
            FailureClass::Authentication
        );
        assert_eq!(
            ProviderError::needs_resync("cannotCalculateChanges").class(),
            FailureClass::NeedsResync
        );
        assert!(ProviderError::retryable("503").is_retryable());
        assert!(!ProviderError::permanent("notFound").is_retryable());
        assert!(ProviderError::needs_resync("reset").requires_resync());
    }

    #[test]
    fn rate_limited_carries_retry_after() {
        let after: Duration = "PT30S".parse().unwrap();
        let err = ProviderError::rate_limited("slow down", Some(after));
        assert_eq!(err.class(), FailureClass::RateLimited);
        assert_eq!(err.retry_after(), Some(after));
        assert!(err.is_retryable());
    }

    #[test]
    fn every_constructor_and_accessor_is_exercised() {
        assert_eq!(
            ProviderError::retryable("a").class(),
            FailureClass::Retryable
        );
        assert_eq!(ProviderError::conflict("b").class(), FailureClass::Conflict);
        let permanent = ProviderError::permanent("c");
        assert_eq!(permanent.detail(), "c");
        assert!(!permanent.requires_resync() && !permanent.is_retryable());
        assert!(ProviderError::needs_resync("d").requires_resync());
        assert!(
            ProviderError::new(FailureClass::InvalidState, "e")
                .retry_after()
                .is_none()
        );
    }

    #[test]
    fn source_is_preserved_and_reachable() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out");
        let err = ProviderError::retryable("transport").with_source(io);
        // Display carries the class + detail; the source chain carries the cause.
        assert!(err.to_string().contains("transport"));
        assert!(std::error::Error::source(&err).is_some());
    }
}
