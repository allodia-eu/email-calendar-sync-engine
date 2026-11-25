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

use engine_core::{error::FailureClass, time::Duration};

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
    confirmation_needed: bool,
    item_unavailable: bool,
    target_refusal: Option<TargetRefusal>,
    credential_failure: bool,
    source: Option<BoxError>,
}

/// Why a target refused to store something into a container, where the reason is the target's
/// own state: what a host must change before sending again, which waiting does not.
///
/// One reason per situation, the same on every adapter and for every kind of object (a message
/// into a mailbox, an event into a calendar, a card into an address book), so a host decides
/// once what each means to it; `providers.md` ("Import") has each adapter's mapping. Built with
/// [`ProviderError::refused_by_target`] and read with [`ProviderError::target_refusal`].
///
/// # Examples
///
/// ```
/// use engine_core::error::FailureClass;
/// use engine_provider::{ProviderError, TargetRefusal};
///
/// let refused = ProviderError::refused_by_target(TargetRefusal::StorageFull);
/// assert_eq!(refused.class(), FailureClass::InvalidState);
/// assert_eq!(refused.target_refusal(), Some(TargetRefusal::StorageFull));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TargetRefusal {
    /// The account's storage is full (IMAP `OVERQUOTA`, JMAP `overQuota` on a store). Freeing or
    /// raising the quota clears it; a rate limit on uploads is a throttle, not this.
    StorageFull,
    /// The container the object names does not exist (a mailbox, a calendar, an address book).
    /// Creating it, or naming the one that does, clears it.
    NoSuchContainer,
    /// The credential may not write into the container (a read-only or shared mailbox, calendar
    /// or address book). Granting the right, or choosing another container, clears it.
    ReadOnlyContainer,
}

impl TargetRefusal {
    /// The engine's own words for the reason, which a refusal carries as its detail.
    fn detail(self) -> &'static str {
        match self {
            Self::StorageFull => "the target refused: the account's storage is full",
            Self::NoSuchContainer => "the target refused: the container does not exist",
            Self::ReadOnlyContainer => "the target refused: the container may not be written",
        }
    }
}

impl ProviderError {
    /// Creates an error with an explicit class and message.
    #[must_use]
    pub fn new(class: FailureClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
            retry_after: None,
            confirmation_needed: false,
            item_unavailable: false,
            target_refusal: None,
            credential_failure: false,
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

    /// The provider can never serve this one item, however often it is asked, while the
    /// account and its other items are unaffected ([`FailureClass::Permanent`]): a message
    /// the server stored but cannot give back, or one a target will never import (too large,
    /// filed in too many mailboxes). A caller may stop asking for it. A permanent
    /// failure without this flag may be about the whole account or the request, and says
    /// nothing about the item.
    #[must_use]
    pub fn item_unavailable(detail: impl Into<String>) -> Self {
        Self {
            item_unavailable: true,
            ..Self::new(FailureClass::Permanent, detail)
        }
    }

    /// The outcome is **genuinely ambiguous** and must not be blindly retried: an SMTP send
    /// whose post-`DATA` acknowledgement was lost, which may or may not have delivered, or a
    /// calendar or contact write whose answer was lost after the server may have applied it
    /// (`providers.md`). Classified [`FailureClass::InvalidState`] so it is never
    /// plain-retryable.
    ///
    /// A send is parked as
    /// [`PendingOutcome::NeedsConfirmation`](engine_core::write::PendingOutcome::NeedsConfirmation)
    /// until sync reconciliation or the user resolves it. A calendar or contact write settles
    /// `Failed`, and its caller is told the change may have been applied. On a write, every
    /// failure that is **not** this one means the write was not applied.
    #[must_use]
    pub fn needs_confirmation(detail: impl Into<String>) -> Self {
        Self {
            confirmation_needed: true,
            ..Self::new(FailureClass::InvalidState, detail)
        }
    }

    /// The target refused because of its own state, for `reason`
    /// ([`FailureClass::InvalidState`]): not retryable as it stands and with no wait to name,
    /// since the target's state must change first. Its detail is the engine's own words for
    /// the reason, never the server's.
    #[must_use]
    pub fn refused_by_target(reason: TargetRefusal) -> Self {
        Self {
            target_refusal: Some(reason),
            ..Self::new(FailureClass::InvalidState, reason.detail())
        }
    }

    /// Why the target refused, where it was its own state
    /// ([`refused_by_target`](Self::refused_by_target)).
    #[must_use]
    pub fn target_refusal(&self) -> Option<TargetRefusal> {
        self.target_refusal
    }

    /// Whether no credential could be had for the operation, so the failure lies with the
    /// credential and not with the server or the way to it: the
    /// [credential source](crate::CredentialSource) failed, or did not answer within the
    /// engine's deadline; it did not [permit](crate::CredentialSource::permits) the origin; or
    /// the engine refused to present the credential, to that origin or in that form. Nothing
    /// went out with a credential, except a request the server had just refused, when the source
    /// then failed to give a replacement.
    ///
    /// The [class](Self::class) is still the source's and still says what to do (a broker that
    /// is down is `Retryable`, consent that is gone is `Authentication`); this says where the
    /// failure lies. A host that renews its connection after a `Retryable` failure, for a
    /// keep-alive socket that died in the meantime, does not renew it for this one: the
    /// connection is sound, and repeating the request at once meets the same source. Set where
    /// the engine asks the source ([`CredentialFailure`](crate::CredentialFailure)), and the
    /// same on every adapter and every operation. A server refusing a credential, or offering
    /// no way to present it, is the server's answer and is not this.
    #[must_use]
    pub fn is_credential_failure(&self) -> bool {
        self.credential_failure
    }

    /// The same failure under `detail`, without its source: every fact a caller reads (the
    /// class, the wait, [`requires_confirmation`](Self::requires_confirmation),
    /// [`is_item_unavailable`](Self::is_item_unavailable),
    /// [`target_refusal`](Self::target_refusal),
    /// [`is_credential_failure`](Self::is_credential_failure)) is kept, and only the words
    /// change. For an adapter that must not pass on a server's words, or that answers several
    /// items with one failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use engine_core::{error::FailureClass, time::Duration};
    /// use engine_provider::ProviderError;
    ///
    /// let wait: Duration = "PT30S".parse().unwrap();
    /// let throttled = ProviderError::rate_limited("the server's own words", Some(wait));
    /// let reworded = throttled.reworded("not stored: HTTP 429");
    /// assert_eq!(reworded.class(), FailureClass::RateLimited);
    /// assert_eq!(reworded.retry_after(), Some(wait));
    /// assert_eq!(reworded.detail(), "not stored: HTTP 429");
    /// ```
    #[must_use]
    pub fn reworded(&self, detail: impl Into<String>) -> Self {
        Self {
            class: self.class,
            detail: detail.into(),
            retry_after: self.retry_after,
            confirmation_needed: self.confirmation_needed,
            item_unavailable: self.item_unavailable,
            target_refusal: self.target_refusal,
            credential_failure: self.credential_failure,
            source: None,
        }
    }

    /// This failure, as one where no credential could be had
    /// ([`is_credential_failure`](Self::is_credential_failure)). Only
    /// [`CredentialFailure`](crate::CredentialFailure) marks one, so the fact has one source.
    pub(crate) fn of_credential(self) -> Self {
        Self {
            credential_failure: true,
            ..self
        }
    }

    /// Attaches the underlying protocol/transport error as the
    /// [`source`](std::error::Error::source).
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

    /// Whether this is an ambiguous outcome that must be confirmed rather than
    /// retried (the outbox records it as `NeedsConfirmation`).
    #[must_use]
    pub fn requires_confirmation(&self) -> bool {
        self.confirmation_needed
    }

    /// Whether the provider can never serve the item asked for
    /// ([`item_unavailable`](Self::item_unavailable)).
    #[must_use]
    pub fn is_item_unavailable(&self) -> bool {
        self.item_unavailable
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

/// The error every unimplemented [`Provider`](crate::Provider) default returns: the
/// adapter does not speak this part of the trait. `what` names the missing half —
/// "mail writes", "calendar sync" — and reads as
/// `provider does not support {what}`.
pub(crate) fn unsupported(what: &str) -> ProviderError {
    ProviderError::invalid_state(format!("provider does not support {what}"))
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
