//! How IMAP and SMTP get the credential they present: asked of the account's
//! [`CredentialSource`] on every dial and every submission (`engine_provider::credential`).
//!
//! Two shapes reach the wire, because servers offer two: a password (IMAP `LOGIN`, SMTP `AUTH
//! PLAIN`) and an OAuth 2.0 access token (SASL `OAUTHBEARER`/`XOAUTH2`, see [`crate::sasl`]).
//! Which SASL mechanism carries the token is negotiated from what the server advertises and is
//! deliberately absent from the credential: a caller that could pin one would be encoding which
//! provider it is talking to.
//!
//! The engine stays **OAuth-agnostic**. Acquiring, storing and refreshing the token is the
//! host's job (`north-star.md`). What this crate owns is *when* it asks: an account's
//! connections are dialled for as long as the host runs, long past the hour a token lives, so
//! every dial asks the source for the credential to present rather than reusing the one the
//! account connected with.

use std::future::Future;

use engine_provider::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource, Origin,
};

use crate::{
    deadline::BOUNDS,
    error::{ImapError, ImapResult},
};

/// What a credential presents over IMAP or SMTP.
pub(crate) enum SignIn<'a> {
    /// IMAP `LOGIN`, SMTP `AUTH PLAIN`.
    Password {
        username: &'a str,
        password: &'a str,
    },
    /// SASL `OAUTHBEARER` or `XOAUTH2`, which both name the account.
    Token { username: &'a str, token: &'a str },
}

/// How `credential` signs in over IMAP or SMTP.
///
/// # Errors
///
/// [`ImapError::Bad`] for a bearer token without the account's username, which both SASL
/// mechanisms need, and for a kind of credential neither protocol can present. Either is the
/// host's mistake, so nothing is sent and no retry will help.
pub(crate) fn sign_in(credential: &Credential) -> ImapResult<SignIn<'_>> {
    match credential {
        Credential::Password { username, password } => Ok(SignIn::Password {
            username,
            password: password.expose(),
        }),
        Credential::Bearer {
            username: Some(username),
            token,
        } => Ok(SignIn::Token {
            username,
            token: token.expose(),
        }),
        Credential::Bearer { username: None, .. } => Err(ImapError::bad(
            "a bearer credential for IMAP or SMTP needs the account's username",
        )),
        _ => Err(ImapError::bad(
            "this kind of credential cannot be presented over IMAP or SMTP",
        )),
    }
}

/// Runs `attempt` with a credential from `source` for `origin`, and once more with a
/// replacement if the server refused it and the source has one
/// ([`engine_provider::with_credential`]).
///
/// The ask is part of the dial, so it is bounded by the dial deadline, and a source that does
/// not answer in time fails the dial as retryable. A refusal is [`ImapError::Auth`]: a `NO` to
/// `LOGIN` or `AUTHENTICATE`, or `535` to SMTP `AUTH`. Never `[LIMIT]`, a temporary code, an
/// SMTP `4xx` or `5xx` other than `535`, or a server offering no mechanism the credential can
/// use ([`ImapError::AuthUnsupported`]): none of those judged the credential, so a fresh one
/// would meet the same answer. Never anything after `AUTH` either, so a replacement repeats no
/// send. `attempt`
/// opens a new connection each time, because a server may close one after a failed sign-in and
/// counts the attempt either way.
pub(crate) async fn with_credential<T, F, Fut>(
    source: &dyn CredentialSource,
    protocol: CredentialProtocol,
    origin: &Origin,
    attempt: F,
) -> ImapResult<T>
where
    F: FnMut(Credential) -> Fut,
    Fut: Future<Output = ImapResult<T>>,
{
    let request = CredentialRequest::new(protocol, origin);
    let is_refusal = |error: &ImapError| matches!(error, ImapError::Auth(_));
    engine_provider::with_credential(source, &request, BOUNDS.dial(), is_refusal, attempt).await
}

impl From<CredentialFailure> for ImapError {
    fn from(failure: CredentialFailure) -> Self {
        Self::Credential(failure.into_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_and_a_named_token_sign_in_as_themselves() {
        let password = Credential::password("alice", "hunter2");
        assert!(matches!(
            sign_in(&password),
            Ok(SignIn::Password {
                username: "alice",
                password: "hunter2"
            })
        ));
        let token = Credential::bearer_for("alice@example.com", "ya29.secret");
        assert!(matches!(
            sign_in(&token),
            Ok(SignIn::Token {
                username: "alice@example.com",
                token: "ya29.secret"
            })
        ));
    }

    #[test]
    #[expect(
        deprecated,
        reason = "code written against the old names must keep compiling"
    )]
    fn the_deprecated_names_still_build_the_same_credentials() {
        let password = crate::Credentials::password("alice", "hunter2");
        assert!(matches!(
            sign_in(&password),
            Ok(SignIn::Password {
                username: "alice",
                ..
            })
        ));
        let token = crate::Credentials::oauth2("alice@example.com", "ya29.secret");
        assert!(matches!(
            sign_in(&token),
            Ok(SignIn::Token {
                username: "alice@example.com",
                token: "ya29.secret"
            })
        ));
        // A fixed credential still configures an account directly.
        let config = crate::ImapConfig::new("h:993", "h", token);
        assert_eq!(config.origin(), Origin::new("imaps", "h", 993));
    }

    #[test]
    fn a_token_without_a_username_cannot_sign_in() {
        // Both SASL mechanisms name the account, so this is the host's mistake: permanent.
        let Err(err) = sign_in(&Credential::bearer("ya29.secret")) else {
            panic!("a token without a username must not sign in");
        };
        assert!(matches!(err, ImapError::Bad(_)), "{err:?}");
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Permanent
        );
        assert!(!err.to_string().contains("ya29.secret"));
    }
}
