//! [`Secret`] and [`Credential`]: what a source hands an adapter to present.

use std::sync::Arc;

use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// A password, app password or access token.
///
/// `Debug` prints `<redacted>`, and there is no `Display` and no `Serialize`: [`expose`] is the
/// only way to read it, and easy to grep for. Clones share one buffer, which is zeroed when the
/// last clone drops.
///
/// Zeroing is best effort. It shortens the life of the copy the engine owns; the copies an
/// adapter derives (a SASL response, an `AUTH PLAIN` token) are its own to wrap, and copies
/// inside an HTTP client, a TLS write buffer or the host's own memory are out of its reach.
///
/// [`expose`]: Self::expose
#[derive(Clone)]
pub struct Secret(Arc<Zeroizing<Box<str>>>);

impl Secret {
    /// Takes `value` as a secret.
    ///
    /// The value is copied into a buffer of exactly its length and the original is zeroed,
    /// because shrinking a `String` into a box reallocates and frees the old buffer as it was.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        let mut value: String = value.into();
        let owned: Box<str> = Box::from(value.as_str());
        value.zeroize();
        Self(Arc::new(Zeroizing::new(owned)))
    }

    /// The secret itself: the only way to read it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Every clone holds the same `Arc`, and the `Zeroizing` inside it zeroes the buffer when the
/// last one drops, so dropping a `Secret` never leaves the buffer behind unzeroed.
impl ZeroizeOnDrop for Secret {}

impl core::fmt::Debug for Secret {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// How an account proves who it is: what a [`CredentialSource`](super::CredentialSource)
/// answers.
///
/// There are no mechanism-specific variants. `XOAUTH2` and `OAUTHBEARER` carry the same username
/// and token, and HTTP Basic and IMAP `LOGIN` the same password, so an adapter picks the
/// mechanism from what the server offers. A caller that could pin one would be saying which
/// provider it talks to.
///
/// `Debug` names the variant and whether a username is present, and nothing else: no secret,
/// and no username either, because a host serving many tenants must not have their addresses
/// appear in a `{:?}`.
#[derive(Clone)]
#[non_exhaustive]
pub enum Credential {
    /// A username and password, or an app password: IMAP `LOGIN`, SMTP `AUTH PLAIN`, HTTP
    /// Basic.
    Password {
        /// The account name the server knows.
        username: String,
        /// The password or app password.
        password: Secret,
    },
    /// An OAuth 2.0 access token.
    Bearer {
        /// The account the token was issued for. IMAP and SMTP need it, as the `user=` of
        /// `XOAUTH2` and the `a=` of `OAUTHBEARER`; HTTP adapters ignore it.
        username: Option<String>,
        /// The access token.
        token: Secret,
    },
}

impl Credential {
    /// A username and password.
    #[must_use]
    pub fn password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Password {
            username: username.into(),
            password: Secret::new(password),
        }
    }

    /// An access token with no username: enough for HTTP, which carries the token alone.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Self::Bearer {
            username: None,
            token: Secret::new(token),
        }
    }

    /// An access token issued for `username`: what IMAP and SMTP need, because both SASL
    /// mechanisms that carry a token name the account too.
    #[must_use]
    pub fn bearer_for(username: impl Into<String>, token: impl Into<String>) -> Self {
        Self::Bearer {
            username: Some(username.into()),
            token: Secret::new(token),
        }
    }

    /// [`bearer_for`](Self::bearer_for) under the name `provider_imap::Credentials` gave it,
    /// kept so that deprecated alias still builds the same credential.
    #[deprecated(note = "use `Credential::bearer_for`")]
    #[must_use]
    pub fn oauth2(username: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self::bearer_for(username, access_token)
    }

    /// The account name, when the credential carries one. It is not a secret, but it is
    /// personal, so `Debug` does not print it.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        match self {
            Self::Password { username, .. } => Some(username),
            Self::Bearer { username, .. } => username.as_deref(),
        }
    }
}

impl core::fmt::Debug for Credential {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (variant, secret) = match self {
            Self::Password { .. } => ("Password", "password"),
            Self::Bearer { .. } => ("Bearer", "token"),
        };
        let username = if self.username().is_some() {
            "<present>"
        } else {
            "<absent>"
        };
        f.debug_struct(variant)
            .field("username", &format_args!("{username}"))
            .field(secret, &format_args!("<redacted>"))
            .finish()
    }
}
