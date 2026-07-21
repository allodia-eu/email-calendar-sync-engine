//! HTTP authentication-scheme negotiation, driven by the server's challenge.
//!
//! JMAP deliberately specifies **no** authentication mechanism of its own: RFC 8620
//! §8.2 points at the IANA HTTP authentication scheme registry, notes that "use of the
//! Basic authentication scheme is NOT RECOMMENDED", and leaves the choice to the
//! server. So a client cannot know the scheme from the credential it was handed — the
//! *server* declares it, in the `WWW-Authenticate` header of its `401` (RFC 9110
//! §11.6.1).
//!
//! That is what this module implements: when a request is rejected with a `401` whose
//! challenge does **not** offer the scheme we used, we re-present the same secret under
//! a scheme the server does offer, and latch it for the rest of the connection. A `401`
//! that *does* offer our scheme is a genuine bad-credential answer and is passed
//! straight through — the retry only ever corrects a scheme mismatch, never masks a
//! wrong password.
//!
//! This is what lets one stored secret work against servers that disagree about the
//! wire format: Stalwart challenges `Basic`, while Fastmail challenges
//! `Bearer resource_metadata="…"` and rejects a Basic header outright ("Invalid
//! Authorization header, not bearer"). Neither is special-cased.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::Credentials;

/// An HTTP authentication scheme this transport can present a credential under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScheme {
    /// `Authorization: Basic base64(username:secret)` (RFC 7617).
    Basic,
    /// `Authorization: Bearer <secret>` (RFC 6750).
    Bearer,
}

impl AuthScheme {
    /// The lowercase scheme token as it appears in a `WWW-Authenticate` challenge.
    fn token(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Bearer => "bearer",
        }
    }

    /// The wire encoding [`NegotiatedScheme`] stores. Never zero, so zero can mean
    /// "nothing latched yet".
    fn as_u8(self) -> u8 {
        match self {
            Self::Basic => 1,
            Self::Bearer => 2,
        }
    }

    /// The inverse of [`AuthScheme::as_u8`]; `None` for the unset encoding.
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Basic),
            2 => Some(Self::Bearer),
            _ => None,
        }
    }
}

/// The scheme a transport is currently presenting its credential under.
///
/// Starts at the credential's natural scheme and moves at most once, when a server
/// challenge proves a different one is required. Latching matters: without it every
/// request against a server that disagrees with the stored credential's shape would pay
/// a wasted round trip.
#[derive(Debug)]
pub(crate) struct NegotiatedScheme(AtomicU8);

impl NegotiatedScheme {
    /// A cell holding `initial`.
    pub(crate) fn new(initial: AuthScheme) -> Self {
        Self(AtomicU8::new(initial.as_u8()))
    }

    /// The scheme to present now.
    ///
    /// `Relaxed` suffices: the value publishes no other memory, and concurrent requests
    /// racing to latch the same server-declared scheme all write the same answer.
    pub(crate) fn get(&self) -> AuthScheme {
        AuthScheme::from_u8(self.0.load(Ordering::Relaxed)).unwrap_or(AuthScheme::Basic)
    }

    /// Latches `scheme` for subsequent requests.
    pub(crate) fn set(&self, scheme: AuthScheme) {
        self.0.store(scheme.as_u8(), Ordering::Relaxed);
    }
}

impl Credentials {
    /// The scheme this credential is naturally presented under, before any server
    /// challenge is seen.
    pub(crate) fn preferred_scheme(&self) -> AuthScheme {
        match self {
            Self::Basic { .. } => AuthScheme::Basic,
            Self::Bearer(_) => AuthScheme::Bearer,
        }
    }

    /// Whether this credential can be presented under `scheme`.
    ///
    /// A Basic credential can also be presented as a bearer token: the secret is the
    /// same opaque string the user was issued (an API token or app password), and only
    /// the framing differs — Basic wraps it with a username, Bearer sends it bare. The
    /// reverse does not hold: a bare token carries no username to build a Basic header
    /// from, so a bearer-only credential stays bearer-only.
    pub(crate) fn can_present(&self, scheme: AuthScheme) -> bool {
        match self {
            // Carries both a username and a secret, so it can be framed either way.
            Self::Basic { .. } => true,
            // A bare token has no username to build a Basic header from.
            Self::Bearer(_) => scheme == AuthScheme::Bearer,
        }
    }

    /// The secret to send as a bearer token.
    pub(crate) fn bearer_secret(&self) -> &str {
        match self {
            Self::Basic { password, .. } => password,
            Self::Bearer(token) => token,
        }
    }
}

/// The scheme to retry `used` as, given a `401`'s challenge headers — or `None` to let
/// the `401` stand.
///
/// Returns `None` when the server offered no challenge (nothing to learn from), when it
/// offered the scheme we already used (the credential is wrong, not its framing), or
/// when nothing it offered can be built from the credential we hold.
///
/// When several offered schemes are presentable, Bearer wins: RFC 8620 §8.2 marks Basic
/// NOT RECOMMENDED, so it is the fallback rather than the preference.
pub(crate) fn negotiate<'a>(
    used: AuthScheme,
    challenges: impl IntoIterator<Item = &'a str>,
    credentials: &Credentials,
) -> Option<AuthScheme> {
    let offered: Vec<String> = challenges.into_iter().flat_map(challenge_schemes).collect();
    if offered.is_empty() {
        return None;
    }
    let offers = |scheme: AuthScheme| offered.iter().any(|s| s == scheme.token());
    if offers(used) {
        return None;
    }
    [AuthScheme::Bearer, AuthScheme::Basic]
        .into_iter()
        .find(|&scheme| offers(scheme) && credentials.can_present(scheme))
}

/// The scheme tokens a `WWW-Authenticate` header value offers, lowercased.
///
/// The header is a comma-separated list of challenges, each a scheme token optionally
/// followed by a `token68` or by comma-separated `name=value` auth-params (RFC 9110
/// §11.6.1). That makes the comma ambiguous — it separates challenges *and* params — so
/// a segment is only read as a new challenge when its first token is a bare word: a
/// `realm="jmap"` continuation carries an `=`, a scheme never does. Commas inside a
/// quoted param value (`realm="a,b"`) are not separators at all.
fn challenge_schemes(header: &str) -> Vec<String> {
    split_outside_quotes(header)
        .into_iter()
        .filter_map(|segment| segment.split_whitespace().next())
        .filter(|first| !first.contains('='))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Splits on commas that are not inside a quoted string, so a quoted auth-param value
/// containing a comma stays one segment.
fn split_outside_quotes(header: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in header.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == ',' && !quoted {
            segments.push(&header[start..index]);
            start = index + 1;
        }
    }
    segments.push(&header[start..]);
    segments
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;
