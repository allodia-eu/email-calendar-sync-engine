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
//! adapters, which all take a bearer token the same way. An expired token surfaces as
//! [`ImapError::Auth`](crate::ImapError::Auth) →
//! [`FailureClass::Authentication`](engine_core::error::FailureClass::Authentication),
//! which is the host's signal to refresh and reconnect.

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
