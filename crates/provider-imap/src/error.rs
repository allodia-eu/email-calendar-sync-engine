//! IMAP/SMTP protocol errors and their classification into the engine taxonomy.
//!
//! [`ImapError`] is the rich protocol-level error (transport I/O, a tagged `NO`/
//! `BAD` completion, an authentication rejection, a malformed response, an
//! unsolicited `BYE`). At the provider-trait boundary it converts into an
//! [`engine_provider::ProviderError`] carrying an engine-neutral [`FailureClass`],
//! so callers branch on the class and never on IMAP specifics (`providers.md`).
//!
//! Note a **UIDVALIDITY change is not an error**: it is a valid server state that
//! the sync layer turns into a snapshot (rediscovery), the IMAP analogue of JMAP
//! `cannotCalculateChanges` — see [`crate::sync`].

use engine_core::error::FailureClass;
use engine_provider::{ProviderError, TargetRefusal};

/// An IMAP (or SMTP) protocol or transport failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImapError {
    /// The underlying socket/TLS stream failed (connect, read, write, handshake).
    #[error("IMAP transport error: {0}")]
    Io(#[from] std::io::Error),

    /// The server refused the credential presented: `LOGIN`/`AUTHENTICATE` returned `NO`, or
    /// SMTP `AUTH` answered `535`. The host must re-auth before the call can succeed, and the
    /// account's credential source is asked once for a replacement.
    #[error("IMAP authentication failed: {0}")]
    Auth(String),

    /// Sign-in failed without the server judging the credential: it offers no mechanism this
    /// credential can use (no OAuth mechanism advertised; SMTP `504`, `534`, `538`), or it
    /// answers with something only the user can resolve (SMTP `432`, a password transition).
    /// Classed as authentication like [`Auth`](Self::Auth), but no replacement is asked for:
    /// another credential of the same kind would meet the same answer.
    #[error("IMAP authentication not possible: {0}")]
    AuthUnsupported(String),

    /// The server refused a sign-in for now rather than judging the credential: `LOGIN`
    /// answered `NO [LIMIT]` (RFC 5530), too many connections or logins, or SMTP `AUTH`
    /// answered a 4xx. Retrying after a backoff can succeed; asking the user to sign in
    /// again cannot help.
    #[error("server is limiting sign-ins for this account: {0}")]
    RateLimited(String),

    /// The account's [`CredentialSource`](crate::CredentialSource) did not permit the server,
    /// had no credential to give, or did not answer within the dial deadline, so nothing was
    /// dialled. Classified as the source classified it; an unanswered ask is retryable.
    #[error("no credential to sign in with: {0}")]
    Credential(ProviderError),

    /// A command returned a tagged `NO`, leaving the session usable. Its response code
    /// decides the class: one RFC 5530 calls temporary (`UNAVAILABLE`, `INUSE`,
    /// `SERVERBUG`) may pass if asked again, and any other `NO` is invalid in the
    /// resource's current state (e.g. `SELECT` of a missing mailbox).
    #[error("IMAP command rejected: {0}")]
    No(String),

    /// A command returned a tagged `BAD`, or a request was malformed: a
    /// client/protocol error that will not succeed unchanged.
    #[error("IMAP protocol error: {0}")]
    Bad(String),

    /// A server response was structurally not what the protocol requires (a
    /// malformed `FETCH`/`LIST`/`ENVELOPE`, an unexpected line). Hostile input is
    /// rejected here, never panicked on (`north-star.md` security).
    #[error("malformed IMAP response: {0}")]
    Protocol(String),

    /// The server sent an unsolicited `* BYE`, closing the connection; reconnecting
    /// may succeed.
    #[error("IMAP server closed the connection: {0}")]
    Bye(String),
}

impl ImapError {
    /// Builds an [`ImapError::Auth`].
    pub(crate) fn auth(detail: impl Into<String>) -> Self {
        Self::Auth(detail.into())
    }

    /// Builds an [`ImapError::AuthUnsupported`].
    pub(crate) fn auth_unsupported(detail: impl Into<String>) -> Self {
        Self::AuthUnsupported(detail.into())
    }

    /// Builds an [`ImapError::RateLimited`].
    pub(crate) fn rate_limited(detail: impl Into<String>) -> Self {
        Self::RateLimited(detail.into())
    }

    /// Builds an [`ImapError::No`].
    pub(crate) fn no(detail: impl Into<String>) -> Self {
        Self::No(detail.into())
    }

    /// Classes a `NO` to `LOGIN` or `AUTHENTICATE`, the two ways into a session.
    ///
    /// It is an authentication failure unless its code says the credential was never judged:
    /// `[LIMIT]` is the server refusing sessions for the account ([`ImapError::RateLimited`]),
    /// and a temporary code stays the retryable `NO` it is (`[UNAVAILABLE]` is RFC 5530 §3's
    /// own example: the directory that checks passwords is down).
    ///
    /// `LIMIT` is read this way only here, because RFC 5530's `LIMIT` is any implementation
    /// limit: on `STORE` it can mean too many keywords, which no wait will fix. Before a
    /// session exists, the only limit left to hit is on sessions.
    pub(crate) fn refused_sign_in(detail: impl Into<String>) -> Self {
        let detail = detail.into();
        if opens_with_code(&detail, "LIMIT") {
            Self::RateLimited(detail)
        } else if opens_with_temporary_code(&detail) {
            Self::No(detail)
        } else {
            Self::Auth(detail)
        }
    }

    /// Builds an [`ImapError::Bad`].
    pub(crate) fn bad(detail: impl Into<String>) -> Self {
        Self::Bad(detail.into())
    }

    /// Builds an [`ImapError::Protocol`].
    pub(crate) fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol(detail.into())
    }

    /// Builds an [`ImapError::Bye`].
    pub(crate) fn bye(detail: impl Into<String>) -> Self {
        Self::Bye(detail.into())
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            // Connection/read/write/handshake failures are transient.
            Self::Io(_) | Self::Bye(_) => FailureClass::Retryable,
            Self::Auth(_) | Self::AuthUnsupported(_) => FailureClass::Authentication,
            Self::RateLimited(_) => FailureClass::RateLimited,
            Self::Credential(source) => source.class(),
            Self::No(detail) if opens_with_temporary_code(detail) => FailureClass::Retryable,
            // Any other `NO` means "not in this state": recompute, do not blind-retry.
            Self::No(_) => FailureClass::InvalidState,
            // `BAD`/malformed is a protocol-level incompatibility: the same request
            // will not start working.
            Self::Bad(_) | Self::Protocol(_) => FailureClass::Permanent,
        }
    }
}

impl From<ImapError> for ProviderError {
    fn from(err: ImapError) -> Self {
        if let Some(reason) = err.target_refusal() {
            return ProviderError::refused_by_target(reason).with_source(err);
        }
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
}

impl ImapError {
    /// The neutral reason a `NO` names by its response code, where the target's own state
    /// refused (`providers.md`, "Import"): `OVERQUOTA` (RFC 9208), a folder that does not exist
    /// (`TRYCREATE`, RFC 3501; `NONEXISTENT`, RFC 5530), and a folder this credential may not
    /// write (`NOPERM`, RFC 5530). Each is already `InvalidState`; this names which.
    fn target_refusal(&self) -> Option<TargetRefusal> {
        let Self::No(detail) = self else {
            return None;
        };
        let named = |code: &str| opens_with_code(detail, code);
        if named("OVERQUOTA") {
            Some(TargetRefusal::StorageFull)
        } else if named("TRYCREATE") || named("NONEXISTENT") {
            Some(TargetRefusal::NoSuchContainer)
        } else if named("NOPERM") {
            Some(TargetRefusal::ReadOnlyContainer)
        } else {
            None
        }
    }
}

/// Whether a `NO`'s text opens with a response code that says the refusal is temporary: the
/// server is short of something (`UNAVAILABLE`), another session holds what the command needs
/// (`INUSE`), or the server failed on its own account (`SERVERBUG`), RFC 5530 §3. The command
/// took no effect, so asking again later is safe and may pass.
///
/// `LIMIT` is not among them: it is any implementation limit, so on `STORE` it can mean too
/// many keywords, which no wait fixes. Only a sign-in reads it as a limit on sessions.
pub(crate) fn opens_with_temporary_code(detail: &str) -> bool {
    ["UNAVAILABLE", "INUSE", "SERVERBUG"]
        .iter()
        .any(|code| opens_with_code(detail, code))
}

/// Whether a completion's text opens with the argument-less response code `code`. A code
/// is only a code at the start of the text (RFC 3501 `resp-text`), and it is an atom, so
/// case does not matter.
pub(crate) fn opens_with_code(detail: &str, code: &str) -> bool {
    detail
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(found, _)| found.eq_ignore_ascii_case(code))
}

/// The result type the IMAP client's internal operations return.
pub(crate) type ImapResult<T> = Result<T, ImapError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_maps_to_its_class() {
        assert_eq!(
            ImapError::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "t")).failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            ImapError::auth("LOGIN NO").failure_class(),
            FailureClass::Authentication
        );
        assert_eq!(
            ImapError::auth_unsupported("no OAuth mechanism").failure_class(),
            FailureClass::Authentication
        );
        assert_eq!(
            ImapError::rate_limited("[LIMIT] LOGIN").failure_class(),
            FailureClass::RateLimited
        );
        assert_eq!(
            ImapError::no("SELECT nonexistent").failure_class(),
            FailureClass::InvalidState
        );
        assert_eq!(
            ImapError::no("[INUSE] SELECT").failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            ImapError::bad("syntax").failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            ImapError::protocol("garbled FETCH").failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            ImapError::bye("shutting down").failure_class(),
            FailureClass::Retryable
        );
    }

    #[test]
    fn converts_into_classified_provider_error_with_source() {
        let provider: ProviderError = ImapError::auth("bad password").into();
        assert_eq!(provider.class(), FailureClass::Authentication);
        assert!(std::error::Error::source(&provider).is_some());

        // A malformed response is permanent and carries its detail through Display.
        let provider: ProviderError = ImapError::protocol("no UIDVALIDITY").into();
        assert_eq!(provider.class(), FailureClass::Permanent);
        assert!(provider.to_string().contains("no UIDVALIDITY"));
    }

    #[test]
    fn io_errors_convert_via_from() {
        // The `#[from]` lets `?` lift a socket error straight into the protocol type.
        let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
        let err: ImapError = io.into();
        assert!(matches!(err, ImapError::Io(_)));
        assert_eq!(err.failure_class(), FailureClass::Retryable);
    }

    /// The response codes that say the target's state refused (RFC 5530 `OVERQUOTA` and
    /// `NOPERM`, RFC 3501 `TRYCREATE`, RFC 5530 `NONEXISTENT`) become the neutral reason, under
    /// the engine's own words rather than the server's text.
    #[test]
    fn a_refusal_by_the_target_s_state_is_named() {
        for (text, reason) in [
            (
                "[OVERQUOTA] Mailbox of Secret Person is full",
                TargetRefusal::StorageFull,
            ),
            (
                "[TRYCREATE] No folder Secret",
                TargetRefusal::NoSuchContainer,
            ),
            (
                "[NONEXISTENT] No folder Secret",
                TargetRefusal::NoSuchContainer,
            ),
            (
                "[NOPERM] Secret is read-only",
                TargetRefusal::ReadOnlyContainer,
            ),
            (
                "[noperm] Secret is read-only",
                TargetRefusal::ReadOnlyContainer,
            ),
        ] {
            let refused = ProviderError::from(ImapError::no(text));
            assert_eq!(refused.target_refusal(), Some(reason), "{text}");
            assert_eq!(refused.class(), FailureClass::InvalidState, "{text}");
            assert!(!refused.detail().contains("Secret"), "{refused}");
        }
        for other in ["SELECT failed", "[INUSE] busy", "Secret [OVERQUOTA] later"] {
            assert_eq!(
                ProviderError::from(ImapError::no(other)).target_refusal(),
                None
            );
        }
        assert_eq!(
            ProviderError::from(ImapError::bad("[NOPERM] x")).target_refusal(),
            None,
            "a BAD is the request's fault, whatever it says"
        );
    }
}
