//! How a connection authenticates, and what it learns from a server's challenge.
//!
//! Split from the transport so the one decision a refusal can change lives beside the
//! credentials it changes, and so `transport.rs` stays under the line limit.

use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::{RequestBuilder, header::WWW_AUTHENTICATE};

/// How a host authenticates to the CalDAV server.
#[derive(Clone)]
#[non_exhaustive]
pub enum Credentials {
    /// HTTP Basic auth (RFC 7617) — the common CalDAV case.
    Basic {
        /// The user name.
        username: String,
        /// The password.
        password: String,
    },
    /// An OAuth 2.0 bearer token (RFC 6750), for providers that require it.
    Bearer(String),
    /// An OAuth 2.0 access token for `username`'s account, sent as RFC 6750 Bearer until
    /// the server's challenge says otherwise.
    ///
    /// A `401` whose `WWW-Authenticate` offers `Basic` and not `Bearer` means the server
    /// reads the token as a password, which some DAV servers that sit beside an OAuth mail
    /// server do. The refused request is sent once more with the token as the Basic
    /// password for `username`, and the connection keeps doing so. A `401` that offers
    /// `Bearer` means the token itself was refused, and nothing is retried.
    Token {
        /// The account the token was issued for, used only once the server asks for Basic.
        username: String,
        /// The access token.
        token: String,
    },
}

impl core::fmt::Debug for Credentials {
    /// Redacts the secret: credentials must never reach logs (`north-star.md`).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"<redacted>")
                .finish(),
            Self::Bearer(_) => f.debug_tuple("Bearer").field(&"<redacted>").finish(),
            Self::Token { username, .. } => f
                .debug_struct("Token")
                .field("username", username)
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// A connection's credentials and what it has learnt about presenting them.
pub(crate) struct Presenter {
    credentials: Credentials,
    /// Set once a server's challenge asked for Basic where a [`Credentials::Token`] was
    /// sent as Bearer. Never cleared: the connection talks to one server.
    token_as_basic: AtomicBool,
}

impl Presenter {
    pub(crate) fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            token_as_basic: AtomicBool::new(false),
        }
    }

    /// Adds this connection's `Authorization` to `builder`.
    pub(crate) fn apply(&self, builder: RequestBuilder) -> RequestBuilder {
        match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
            Credentials::Token { username, token } => {
                if self.token_as_basic.load(Ordering::Relaxed) {
                    builder.basic_auth(username, Some(token))
                } else {
                    builder.bearer_auth(token)
                }
            }
        }
    }

    /// Whether `response` refused a token the server would take as a Basic password, in
    /// which case every later request presents it that way and the caller sends the refused
    /// one again.
    pub(crate) fn learn(&self, response: &reqwest::Response) -> bool {
        if response.status() != reqwest::StatusCode::UNAUTHORIZED
            || !matches!(self.credentials, Credentials::Token { .. })
            || self.token_as_basic.load(Ordering::Relaxed)
        {
            return false;
        }
        let schemes: Vec<String> = response
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(challenge_schemes)
            .collect();
        let asks_for_basic = schemes.iter().any(|scheme| scheme == "basic")
            && !schemes.iter().any(|scheme| scheme == "bearer");
        if asks_for_basic {
            self.token_as_basic.store(true, Ordering::Relaxed);
        }
        asks_for_basic
    }
}

/// The auth schemes a `WWW-Authenticate` field value offers, lowercased (RFC 9110 §11.6.1).
///
/// A field holds comma-separated challenges whose parameters are comma-separated too, so a
/// list element is a new challenge exactly when its first token is not followed by `=`:
/// `charset="UTF-8"` after `Basic realm="x"` is Basic's parameter, not a scheme. Commas
/// inside quoted strings separate nothing.
pub(crate) fn challenge_schemes(value: &str) -> Vec<String> {
    let mut elements = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (index, byte) in value.bytes().enumerate() {
        match byte {
            _ if escaped => escaped = false,
            b'\\' if quoted => escaped = true,
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                elements.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    elements.push(&value[start..]);
    elements
        .into_iter()
        .filter_map(|element| {
            let element = element.trim_start();
            let end = element
                .find(|c: char| c.is_ascii_whitespace() || c == '=')
                .unwrap_or(element.len());
            let (scheme, rest) = element.split_at(end);
            let is_parameter = rest.trim_start().starts_with('=');
            (!scheme.is_empty() && !is_parameter).then(|| scheme.to_ascii_lowercase())
        })
        .collect()
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;
