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
    credential_failure: Option<CredentialFailureKind>,
    source: Option<BoxError>,
}

/// Where a failure to have a credential lies ([`ProviderError::credential_failure`]): what a
/// host must change, which the [class](ProviderError::class) cannot say. One kind per situation,
/// the same on every adapter and every protocol.
///
/// # Examples
///
/// ```
/// use engine_provider::{CredentialFailure, CredentialFailureKind, ProviderError};
///
/// let refused: ProviderError = CredentialFailure::refused_origin("plaintext off loopback").into();
/// assert_eq!(
///     refused.credential_failure(),
///     Some(CredentialFailureKind::Insecure)
/// );
/// assert!(refused.is_credential_failure());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CredentialFailureKind {
    /// The [credential source](crate::CredentialSource) failed, or did not answer within the
    /// engine's deadline. Its [class](ProviderError::class) says what to do: a broker or token
    /// endpoint that is down is `Retryable`, consent that is gone is `Authentication`, a
    /// throttled token endpoint is `RateLimited`.
    Source,
    /// The credential would leave TLS because the address the account is configured with is
    /// plaintext HTTP off loopback. The engine's own rule, which no source can loosen. A secure
    /// (`https`) address for the account clears it; no new password or sign-in can.
    Insecure,
    /// The credential would leave TLS on an address other than the configured one, from an
    /// account whose configured address the rule accepts (`https`, or plaintext loopback): a
    /// redirect from HTTPS to plaintext on another host or port, or back to the URL that issued
    /// it; a discovery step, a session or a link naming plaintext HTTP off loopback, or a
    /// CalDAV shared calendar's principal the host stored in plaintext; or plaintext loopback
    /// named to an account not configured there. The engine's own rule, as for
    /// [`Insecure`](Self::Insecure), but the configured address itself passes it. Usually the
    /// server's operator clears it, by serving that address over TLS (the address is not in the
    /// error). Pointing the account straight at a secure address for the same service, where
    /// one exists (the `https` form of a host a well-known address redirects to in plaintext),
    /// or replacing a stored principal (by discovering again), clears it too. No new password
    /// or sign-in can.
    LedToInsecure,
    /// The host's policy does not let the credential go to the origin the request is for: the
    /// source did not [permit](crate::CredentialSource::permits) it, or a host's deprecated
    /// credential-host policy refused it. The origin may be the address the account is
    /// configured with, or one the server led to (a redirect, or a discovery step naming another
    /// host, such as a calendar home on a host of its own); the refused address is not in the
    /// error. Changing the host's policy, or the configured address where that is the one
    /// refused, clears it; no new password or sign-in can.
    NotPermitted,
    /// The credential is in a form the protocol cannot carry: a kind it cannot present (an
    /// administrator acting as another account over HTTP, a password to a token-only API, a
    /// token without the account's username over SASL), or a byte it cannot frame (CR, LF or NUL
    /// in an IMAP `LOGIN`, a token outside visible ASCII, a Basic username holding a colon).
    /// Changing the stored credential clears it; asking again gives the same one.
    Form,
    /// The source answered for another account than the one the adapter's connections sign in
    /// as: a host's mistake (a source answering for the wrong subject, or accounts mixed up).
    Account,
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
            credential_failure: None,
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
        self.credential_failure.is_some()
    }

    /// Where the failure to have a credential lies, where it is one
    /// ([`is_credential_failure`](Self::is_credential_failure)): the source, the origin, the
    /// credential's form, or the account. The class says what to do about the source's own
    /// failure; the kind says what a person must change for the others, so a host can word its
    /// answer without reading the detail. Set by the same
    /// [`CredentialFailure`](crate::CredentialFailure) constructors that mark the failure.
    #[must_use]
    pub fn credential_failure(&self) -> Option<CredentialFailureKind> {
        self.credential_failure
    }

    /// Whether this failure, met by one item of a batch, reaches every item after it, so the
    /// batch ends here rather than send (or ask the credential source) again for each: a
    /// throttle, a credential the server refused, or no credential at all
    /// ([`is_credential_failure`](Self::is_credential_failure), whatever its class: a source
    /// that is down or hung would cost every later item another ask and its deadline). A failure
    /// about one item, or about one exchange, ends nothing.
    ///
    /// The one rule for every loop that sends item by item: an import's batch
    /// (`import::not_sent` answers the items after it), and a
    /// rule-by-rule write of an account's rules.
    ///
    /// # Examples
    ///
    /// ```
    /// use engine_provider::ProviderError;
    ///
    /// assert!(ProviderError::rate_limited("429", None).ends_batch());
    /// assert!(!ProviderError::item_unavailable("too large").ends_batch());
    /// ```
    #[must_use]
    pub fn ends_batch(&self) -> bool {
        self.credential_failure.is_some()
            || matches!(
                self.class,
                FailureClass::RateLimited | FailureClass::Authentication
            )
    }

    /// The same failure under `detail`, without its source: every fact a caller reads (the
    /// class, the wait, [`requires_confirmation`](Self::requires_confirmation),
    /// [`is_item_unavailable`](Self::is_item_unavailable),
    /// [`target_refusal`](Self::target_refusal),
    /// [`credential_failure`](Self::credential_failure)) is kept, and only the words
    /// change. For an adapter that must not pass on a server's words, or that answers several
    /// items with one failure of the whole request or session: since it keeps
    /// [`is_item_unavailable`](Self::is_item_unavailable) and
    /// [`requires_confirmation`](Self::requires_confirmation), a failure about one item is never
    /// fanned out to the others with it.
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

    /// This failure, as one where no credential could be had, for the reason `kind` names
    /// ([`credential_failure`](Self::credential_failure)). Only
    /// [`CredentialFailure`](crate::CredentialFailure) marks one, so the fact has one source.
    ///
    /// Only the class, the words, the wait and the source chain carry over. The other facts are
    /// about a request that went out (may it have been applied, will the target ever take this
    /// item, did its state refuse), and nothing went out: a source answering
    /// [`needs_confirmation`](Self::needs_confirmation) must not park a send that never left.
    pub(crate) fn of_credential(self, kind: CredentialFailureKind) -> Self {
        Self {
            confirmation_needed: false,
            item_unavailable: false,
            target_refusal: None,
            credential_failure: Some(kind),
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
