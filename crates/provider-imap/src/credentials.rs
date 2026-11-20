//! [`Credentials`] — what an IMAP/SMTP account authenticates with.
//!
//! Two shapes, because servers offer two: a password (IMAP `LOGIN`, SMTP `AUTH
//! PLAIN`) and an OAuth 2.0 access token (SASL `OAUTHBEARER`/`XOAUTH2`, see
//! [`crate::sasl`]). Which SASL mechanism carries the token is negotiated from what
//! the server advertises and is deliberately absent here: a caller that could pin one
//! would be encoding which provider it is talking to.
//!
//! The engine stays **OAuth-agnostic**. Acquiring, storing and refreshing the token is
//! the host's job (`north-star.md`), exactly as for the JMAP, CalDAV, Graph and Google
//! adapters, which all take a bearer token the same way. What the engine owns is *when* it
//! asks: an account's connections are dialled for as long as the host runs, long past the
//! hour a token lives, so every dial asks the config's [`CredentialSource`] for the
//! credential to present rather than reusing the one the account connected with.

use std::future::Future;

use async_trait::async_trait;
use engine_provider::ProviderError;

use crate::error::{ImapError, ImapResult};

/// How an account proves who it is.
#[derive(Clone)]
#[non_exhaustive]
pub enum Credentials {
    /// A username and password: IMAP `LOGIN`, SMTP `AUTH PLAIN`. Also the shape of an
    /// app-specific password, which several providers issue precisely so a client that
    /// cannot do OAuth still works.
    Password {
        /// The account name the server knows.
        username: String,
        /// The password or app-specific password.
        password: String,
    },
    /// A username and an OAuth 2.0 access token, presented over SASL. Required by
    /// Microsoft (basic auth is switched off for IMAP/SMTP on Exchange Online) and by
    /// Yahoo, and available on Gmail.
    OAuth2 {
        /// The account name — the address the token was issued for, which is what both
        /// mechanisms carry as the authorization identity.
        username: String,
        /// The bearer token. Short-lived: the host refreshes it and reconnects.
        access_token: String,
    },
}

impl Credentials {
    /// A username/password credential.
    #[must_use]
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Password {
            username: username.into(),
            password: password.into(),
        }
    }

    /// An OAuth 2.0 credential: the account address plus a bearer access token.
    #[must_use]
    pub fn oauth2(username: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self::OAuth2 {
            username: username.into(),
            access_token: access_token.into(),
        }
    }

    /// The account name, whichever shape the credential takes — the one component that
    /// is not a secret, so it is the one a `Debug` rendering or a log may name.
    #[must_use]
    pub fn username(&self) -> &str {
        match self {
            Self::Password { username, .. } | Self::OAuth2 { username, .. } => username,
        }
    }
}

/// Redacts the secret, whichever it is. The variant name is kept: knowing an account
/// is on OAuth rather than a password is the first question a support report answers,
/// and it gives nothing away (`north-star.md` security).
impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let variant = match self {
            Self::Password { .. } => "Password",
            Self::OAuth2 { .. } => "OAuth2",
        };
        f.debug_struct(variant)
            .field("username", &self.username())
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Where a connection gets the [`Credentials`] it presents: asked once per IMAP connection
/// the account dials and once per SMTP submission.
///
/// A [`Credentials`] is itself a source that always answers the same, which is all a
/// password account needs. A host whose account signs in with OAuth implements this over
/// its token store, so a dial an hour after connecting presents a token that is valid then.
#[async_trait]
pub trait CredentialSource: Send + Sync + core::fmt::Debug {
    /// The credential to present on the dial about to happen. A token source returns one
    /// that is valid now, refreshing first if its token is near expiry.
    ///
    /// # Errors
    ///
    /// Whatever stops the host producing one, classified as the host sees it: a revoked
    /// grant is [`FailureClass::Authentication`](engine_core::error::FailureClass::Authentication),
    /// an unreachable token endpoint is retryable. The dial fails with
    /// [`ImapError::Credential`] carrying it, and dials nothing.
    async fn credentials(&self) -> Result<Credentials, ProviderError>;

    /// A replacement for `refused`, which the server has just refused, or `None` when no
    /// other is worth presenting. Asked at most once per dial or submission.
    ///
    /// The default is `None`: the same password to the same server gets the same answer, at
    /// a provider that may be counting failures towards a lockout. A token source refreshes
    /// here even when its token has not reached its stated expiry, because the server has
    /// just said it is no longer good.
    ///
    /// # Errors
    ///
    /// As [`credentials`](Self::credentials).
    async fn renew(&self, refused: &Credentials) -> Result<Option<Credentials>, ProviderError> {
        let _ = refused;
        Ok(None)
    }
}

/// A fixed credential is its own source: the same answer every time, and no renewal.
#[async_trait]
impl CredentialSource for Credentials {
    async fn credentials(&self) -> Result<Credentials, ProviderError> {
        Ok(self.clone())
    }
}

/// Runs `attempt` with the source's credential, and once more with a renewed one if the
/// server refused it and the source has a replacement.
///
/// `attempt` must fail with [`ImapError::Auth`] only when the credential was refused, before
/// anything else happened, because a retry repeats the whole attempt. A second refusal is
/// returned as it came: a fresh credential that is also refused is an answer about the
/// server, not about the token.
pub(crate) async fn with_renewal<T, F, Fut>(
    source: &dyn CredentialSource,
    mut attempt: F,
) -> ImapResult<T>
where
    F: FnMut(Credentials) -> Fut,
    Fut: Future<Output = ImapResult<T>>,
{
    let credentials = source.credentials().await.map_err(ImapError::Credential)?;
    match attempt(credentials.clone()).await {
        Err(ImapError::Auth(detail)) => match source
            .renew(&credentials)
            .await
            .map_err(ImapError::Credential)?
        {
            Some(renewed) => attempt(renewed).await,
            None => Err(ImapError::Auth(detail)),
        },
        outcome => outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_either_secret_and_keeps_the_shape() {
        let password = format!("{:?}", Credentials::password("alice", "hunter2"));
        assert!(password.contains("Password") && password.contains("alice"));
        assert!(!password.contains("hunter2"), "password leaked: {password}");

        let oauth = format!(
            "{:?}",
            Credentials::oauth2("alice@example.com", "ya29.secret")
        );
        assert!(oauth.contains("OAuth2") && oauth.contains("alice@example.com"));
        assert!(!oauth.contains("ya29.secret"), "token leaked: {oauth}");
    }

    #[test]
    fn the_username_reads_out_of_either_shape() {
        assert_eq!(Credentials::password("alice", "pw").username(), "alice");
        assert_eq!(
            Credentials::oauth2("bob@example.com", "tok").username(),
            "bob@example.com"
        );
    }
}
