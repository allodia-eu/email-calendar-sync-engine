//! How IMAP and SMTP get the credential they present: asked of the account's
//! [`CredentialSource`] on every dial and every submission (`engine_provider::credential`).
//!
//! Three shapes reach the wire: a password (IMAP `LOGIN`, SMTP `AUTH PLAIN`), an OAuth 2.0
//! access token (SASL `OAUTHBEARER`/`XOAUTH2`, see [`crate::sasl`]), and an administrator's
//! password acting as another account (IMAP `AUTHENTICATE PLAIN` with an authorization
//! identity, RFC 4616; never SMTP).
//! Which SASL mechanism carries the token is negotiated from what the server advertises and is
//! deliberately absent from the credential: a caller that could pin one would be encoding which
//! provider it is talking to.
//!
//! The engine stays **OAuth-agnostic**. Acquiring, storing and refreshing the token is the
//! host's job (`north-star.md`). What this crate owns is *when* it asks: an account's
//! connections are dialled for as long as the host runs, long past the hour a token lives, so
//! every dial asks the source for the credential to present rather than reusing the one the
//! account connected with.

use std::{future::Future, sync::OnceLock};

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
    /// IMAP `AUTHENTICATE PLAIN`: the administrator `username` acting as `acting_as`.
    ActingAs {
        username: &'a str,
        password: &'a str,
        acting_as: &'a str,
    },
}

/// What a credential presents over SMTP: [`SignIn`] without the shapes SMTP refuses, so that
/// the submission's `AUTH` cannot meet one ([`submit_as`]).
pub(crate) enum SubmitAs<'a> {
    /// `AUTH PLAIN`.
    Password {
        username: &'a str,
        password: &'a str,
    },
    /// `AUTH OAUTHBEARER` or `AUTH XOAUTH2`.
    Token { username: &'a str, token: &'a str },
}

/// How `credential` signs in over IMAP.
///
/// # Errors
///
/// [`ImapError::Credential`], classed `Permanent`
/// ([`CredentialFailure::unpresentable`]), for a bearer token without the account's username,
/// which both SASL mechanisms need; for a credential acting as another account that names no
/// account to act as; and for a kind of credential IMAP cannot present. Each is the host's
/// mistake, so nothing is sent and no retry will help.
pub(crate) fn sign_in(credential: &Credential) -> ImapResult<SignIn<'_>> {
    match credential {
        Credential::Password { username, password } => Ok(SignIn::Password {
            username,
            password: password.expose(),
        }),
        // An empty authorization identity means "the authentication identity's own" (RFC 4616
        // §2), which would open the administrator's mailbox in place of the one asked for.
        Credential::ActingAs { acting_as, .. } if acting_as.is_empty() => {
            Err(CredentialFailure::unpresentable(
                "a credential acting as another account names no account to act as",
            )
            .into())
        }
        Credential::ActingAs {
            username,
            password,
            acting_as,
        } => Ok(SignIn::ActingAs {
            username,
            password: password.expose(),
            acting_as,
        }),
        Credential::Bearer {
            username: Some(username),
            token,
        } => Ok(SignIn::Token {
            username,
            token: token.expose(),
        }),
        Credential::Bearer { username: None, .. } => Err(CredentialFailure::unpresentable(
            "a bearer credential for IMAP or SMTP needs the account's username",
        )
        .into()),
        _ => Err(CredentialFailure::unpresentable(
            "this kind of credential cannot be presented over IMAP or SMTP",
        )
        .into()),
    }
}

/// How `credential` authenticates a submission over SMTP.
///
/// # Errors
///
/// As [`sign_in`], and [`ImapError::Credential`] (`Permanent`) for a credential acting as
/// another account:
/// submitting mail as another account is a different grant from reading its mailbox, and no
/// server has been seen to take it, so it is refused rather than guessed at.
pub(crate) fn submit_as(credential: &Credential) -> ImapResult<SubmitAs<'_>> {
    match sign_in(credential)? {
        SignIn::Password { username, password } => Ok(SubmitAs::Password { username, password }),
        SignIn::Token { username, token } => Ok(SubmitAs::Token { username, token }),
        SignIn::ActingAs { .. } => Err(CredentialFailure::unpresentable(
            "a credential acting as another account is presented over IMAP only",
        )
        .into()),
    }
}

/// The failure of a dial whose credential names another account than the one it must sign in
/// as: [`ImapError::Credential`], classed `InvalidState`
/// ([`CredentialFailure::another_account`]). It names neither.
pub(crate) fn another_account() -> ImapError {
    CredentialFailure::another_account(
        "the credential source answered for another account than the one this account signs in \
         as",
    )
    .into()
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

/// The account an account's connections sign in as, remembered from the first dial that
/// signed in, so that a later credential naming another account fails its dial before anything
/// is sent (`docs/design/credential-source.md`, "Direct host").
///
/// Every connection in one pool must belong to one mailbox: the pool hands them out
/// interchangeably, so a connection signed in as another account would serve that account's
/// mail to whoever borrowed it. That would be a host bug (a source answering for the wrong
/// subject), and this check makes it a failed dial instead of a silent mix. The identity is
/// the account whose mailbox opens ([`SignIn::identity`]): for a credential acting as another
/// account, that account, not the administrator ([`Credential::authorization_identity`]).
/// Compared exactly, byte for byte as the server was sent it: `Alice@example.com` and
/// `alice@example.com` are two accounts here, even where the server folds case.
#[derive(Default)]
pub(crate) struct PresentedIdentity(OnceLock<Box<str>>);

impl PresentedIdentity {
    /// Whether a dial may present `identity`: always before the first sign-in, and afterwards
    /// only the identity that signed in.
    ///
    /// # Errors
    ///
    /// [`ImapError::Credential`], classed `InvalidState`, for another account.
    /// It names neither account.
    pub(crate) fn check(&self, identity: &str) -> ImapResult<()> {
        match self.0.get() {
            Some(known) if **known != *identity => Err(another_account()),
            _ => Ok(()),
        }
    }

    /// Remembers `identity` once a dial has signed in with it; the first one to sign in stays.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check), when another dial signed in first as another account, so
    /// that two first dials racing each other cannot both join the pool.
    pub(crate) fn remember(&self, identity: &str) -> ImapResult<()> {
        let known = self.0.get_or_init(|| identity.into());
        if **known == *identity {
            Ok(())
        } else {
            Err(another_account())
        }
    }
}

impl core::fmt::Debug for PresentedIdentity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let known = if self.0.get().is_some() {
            "<present>"
        } else {
            "<absent>"
        };
        f.debug_tuple("PresentedIdentity")
            .field(&format_args!("{known}"))
            .finish()
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
        assert!(matches!(err, ImapError::Credential(_)), "{err:?}");
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::Permanent
        );
        assert!(!err.to_string().contains("ya29.secret"));
    }

    const MASTER: &str = "master@example.com";
    const ALICE: &str = "alice@example.com";

    #[test]
    fn an_administrator_acting_as_an_account_signs_in_over_imap_as_that_account() {
        let credential = Credential::acting_as(MASTER, "master-pw", ALICE);
        let signed = sign_in(&credential).expect("IMAP presents it");
        assert!(matches!(
            signed,
            SignIn::ActingAs {
                username: MASTER,
                password: "master-pw",
                acting_as: ALICE
            }
        ));
    }

    #[test]
    fn acting_as_another_account_is_refused_over_smtp_and_without_an_account() {
        let over_smtp = submit_as(&Credential::acting_as(MASTER, "master-pw", ALICE)).err();
        let nobody = sign_in(&Credential::acting_as(MASTER, "master-pw", "")).err();
        for err in [over_smtp, nobody] {
            let err = err.expect("refused");
            assert!(matches!(err, ImapError::Credential(_)), "{err:?}");
            assert_eq!(
                err.failure_class(),
                engine_core::error::FailureClass::Permanent
            );
            for leaked in [MASTER, ALICE, "master-pw"] {
                assert!(!err.to_string().contains(leaked), "{err}");
            }
        }
        // What SMTP can present, it still does.
        let password = Credential::password(ALICE, "pw");
        assert!(matches!(
            submit_as(&password),
            Ok(SubmitAs::Password {
                username: ALICE,
                password: "pw"
            })
        ));
        let token = Credential::bearer_for(ALICE, "tok");
        assert!(matches!(
            submit_as(&token),
            Ok(SubmitAs::Token {
                username: ALICE,
                token: "tok"
            })
        ));
    }

    #[test]
    fn a_dial_may_present_only_the_identity_that_signed_in_first() {
        let presented = PresentedIdentity::default();
        // Before anything signed in, any account may.
        presented
            .check("bob@example.com")
            .expect("nothing remembered yet");
        presented.remember(ALICE).expect("the first to sign in");
        presented.check(ALICE).expect("the same account again");

        let err = presented
            .check("bob@example.com")
            .expect_err("another account");
        assert!(matches!(err, ImapError::Credential(_)), "{err:?}");
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::InvalidState
        );
        assert!(!err.to_string().contains("example.com"), "{err}");
        assert_eq!(format!("{presented:?}"), "PresentedIdentity(<present>)");

        // The first identity stays: a later sign-in as another account fails rather than
        // joining, as a dial racing the first one would.
        let err = presented
            .remember("bob@example.com")
            .expect_err("another account");
        assert_eq!(
            err.failure_class(),
            engine_core::error::FailureClass::InvalidState
        );
        presented.remember(ALICE).expect("the first one again");
        presented.check(ALICE).expect("still the first one");
    }
}
