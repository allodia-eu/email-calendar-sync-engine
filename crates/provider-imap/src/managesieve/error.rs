//! How a ManageSieve session fails, and the one place that classifies it.
//!
//! A failure below the protocol (the socket, TLS, a malformed answer, a refused sign-in) is the
//! same [`ImapError`] the IMAP session raises, classified the same way. A command the server
//! refused is [`SieveError::Refused`], carrying the command and its response code and nothing
//! the server wrote: ManageSieve's `NO` prose can quote the script (Dovecot's names its line
//! numbers and the commands in it), so it never reaches an error.

use engine_core::error::FailureClass;
use engine_provider::{CredentialFailure, ProviderError, TargetRefusal};

use super::response::{Quota, ResponseCode};
use crate::error::ImapError;

/// A ManageSieve failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SieveError {
    /// The transport, TLS, sign-in, or a malformed answer, as IMAP classes them.
    #[error(transparent)]
    Imap(#[from] ImapError),
    /// The server answered `NO` to `command`, with `code` where it gave one.
    #[error("the ManageSieve server refused {command}{}", code_suffix(*.code))]
    Refused {
        /// The command, as the protocol names it.
        command: &'static str,
        /// The response code, where the server gave one.
        code: Option<ResponseCode>,
    },
}

fn code_suffix(code: Option<ResponseCode>) -> String {
    code.map(|code| format!(" ({code})")).unwrap_or_default()
}

/// The result of a ManageSieve operation.
pub(crate) type SieveResult<T> = Result<T, SieveError>;

impl From<std::io::Error> for SieveError {
    fn from(error: std::io::Error) -> Self {
        Self::Imap(ImapError::Io(error))
    }
}

impl From<CredentialFailure> for SieveError {
    fn from(failure: CredentialFailure) -> Self {
        Self::Imap(failure.into())
    }
}

impl SieveError {
    /// Whether the server refused the credential presented, so a replacement may be asked for.
    pub(crate) fn is_sign_in_refusal(&self) -> bool {
        matches!(self, Self::Imap(ImapError::Auth(_)))
    }

    /// The response code of a refused command.
    #[cfg(test)]
    pub(crate) fn code(&self) -> Option<ResponseCode> {
        match self {
            Self::Refused { code, .. } => *code,
            Self::Imap(_) => None,
        }
    }
}

impl From<SieveError> for ProviderError {
    /// A refusal is classed by its code (RFC 5804 §1.3): `TRYLATER` may pass if asked again; a
    /// quota on the number of scripts or the account's space is the target's own state, which
    /// waiting does not clear (`StorageFull`); anything else is the account's state refusing
    /// this command (`InvalidState`). A `QUOTA/MAXSIZE` on a script being stored is the
    /// script's size, which the rules write classes itself before this.
    fn from(error: SieveError) -> Self {
        let code = match error {
            SieveError::Imap(error) => return error.into(),
            SieveError::Refused { code, .. } => code,
        };
        match code {
            Some(ResponseCode::Quota(Quota::Unspecified | Quota::MaxScripts)) => {
                ProviderError::refused_by_target(TargetRefusal::StorageFull).with_source(error)
            }
            Some(ResponseCode::TryLater) => {
                ProviderError::new(FailureClass::Retryable, error.to_string()).with_source(error)
            }
            _ => {
                ProviderError::new(FailureClass::InvalidState, error.to_string()).with_source(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_its_command_and_code_and_nothing_else() {
        let refused = SieveError::Refused {
            command: "GETSCRIPT",
            code: Some(ResponseCode::Nonexistent),
        };
        assert_eq!(
            refused.to_string(),
            "the ManageSieve server refused GETSCRIPT (NONEXISTENT)"
        );
        let bare = SieveError::Refused {
            command: "PUTSCRIPT",
            code: None,
        };
        assert_eq!(bare.to_string(), "the ManageSieve server refused PUTSCRIPT");
    }

    #[test]
    fn a_refusal_is_classed_by_its_code() {
        let class = |code| {
            ProviderError::from(SieveError::Refused {
                command: "PUTSCRIPT",
                code,
            })
        };
        assert_eq!(
            class(Some(ResponseCode::TryLater)).class(),
            FailureClass::Retryable
        );
        assert_eq!(class(None).class(), FailureClass::InvalidState);
        assert_eq!(
            class(Some(ResponseCode::Active)).class(),
            FailureClass::InvalidState
        );
        for quota in [Quota::Unspecified, Quota::MaxScripts] {
            assert_eq!(
                class(Some(ResponseCode::Quota(quota))).target_refusal(),
                Some(TargetRefusal::StorageFull)
            );
        }
        assert_eq!(
            class(Some(ResponseCode::Quota(Quota::MaxSize))).target_refusal(),
            None,
            "a script too large is the script's fault, not the account's state"
        );
    }

    #[test]
    fn only_a_refused_credential_asks_for_another() {
        assert!(SieveError::Imap(ImapError::auth("refused")).is_sign_in_refusal());
        assert!(!SieveError::Imap(ImapError::bye("closing")).is_sign_in_refusal());
        assert!(
            !SieveError::Refused {
                command: "AUTHENTICATE",
                code: Some(ResponseCode::TryLater)
            }
            .is_sign_in_refusal()
        );
    }
}
