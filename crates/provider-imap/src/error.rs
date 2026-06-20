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
use engine_provider::ProviderError;

/// An IMAP (or SMTP) protocol or transport failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImapError {
    /// The underlying socket/TLS stream failed (connect, read, write, handshake).
    #[error("IMAP transport error: {0}")]
    Io(#[from] std::io::Error),

    /// Authentication was rejected (`LOGIN`/`AUTHENTICATE` returned `NO`, or SMTP
    /// `AUTH` failed). The host must re-auth before the call can succeed.
    #[error("IMAP authentication failed: {0}")]
    Auth(String),

    /// A command returned a tagged `NO`: it is invalid in the resource's current
    /// state (e.g. `SELECT` of a missing mailbox), not retryable as-is.
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

    /// Builds an [`ImapError::No`].
    pub(crate) fn no(detail: impl Into<String>) -> Self {
        Self::No(detail.into())
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
            Self::Auth(_) => FailureClass::Authentication,
            // `NO` means "not now, in this state" — recompute, do not blind-retry.
            Self::No(_) => FailureClass::InvalidState,
            // `BAD`/malformed is a protocol-level incompatibility: the same request
            // will not start working.
            Self::Bad(_) | Self::Protocol(_) => FailureClass::Permanent,
        }
    }
}

impl From<ImapError> for ProviderError {
    fn from(err: ImapError) -> Self {
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
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
            ImapError::no("SELECT nonexistent").failure_class(),
            FailureClass::InvalidState
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
}
