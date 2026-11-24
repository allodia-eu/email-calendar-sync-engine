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
//! that *does* offer our scheme is a genuine bad-credential answer: the re-framing never
//! masks a wrong password, and only then is the account's credential source asked for a
//! replacement (`engine_http::send_authenticated`). One `401` gets one of the two, never both.
//!
//! A challenge offering only schemes the credential cannot be framed under (Basic alone, to a
//! token) is final: the source is not asked for a replacement, because another token would be
//! refused the same way, and every request would refresh the token against a server that never
//! takes Bearer.
//!
//! This is what lets one stored secret work against servers that disagree about the
//! wire format: Stalwart challenges `Bearer` and `Basic`, in two headers (v1.0.2), so a wrong
//! password presented as Basic is refused as it stands, while Fastmail challenges
//! `Bearer resource_metadata="…"` alone and rejects a Basic header outright ("Invalid
//! Authorization header, not bearer"). Neither is special-cased.

use core::sync::atomic::{AtomicU8, Ordering};

use engine_provider::Credential;

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

/// The scheme a transport presents credentials under, once a server has said which.
///
/// Empty until a challenge proves the credential's natural scheme wrong, and then latched:
/// without it every request against a server that disagrees with the credential's shape would
/// pay a wasted round trip. It describes the server, not one credential, so it survives the
/// source handing out a new one; a credential that cannot be framed under it is presented
/// under its own ([`scheme_for`](Self::scheme_for)).
#[derive(Debug, Default)]
pub(crate) struct NegotiatedScheme(AtomicU8);

impl NegotiatedScheme {
    /// The scheme latched, if any.
    ///
    /// `Relaxed` suffices: the value publishes no other memory, and concurrent requests
    /// racing to latch the same server-declared scheme all write the same answer.
    pub(crate) fn latched(&self) -> Option<AuthScheme> {
        AuthScheme::from_u8(self.0.load(Ordering::Relaxed))
    }

    /// Latches `scheme` for subsequent requests.
    pub(crate) fn set(&self, scheme: AuthScheme) {
        self.0.store(scheme.as_u8(), Ordering::Relaxed);
    }

    /// The scheme held, leaving none.
    pub(crate) fn take(&self) -> Option<AuthScheme> {
        AuthScheme::from_u8(self.0.swap(0, Ordering::Relaxed))
    }

    /// The scheme to present `credential` under now: the latched one where the credential
    /// can be framed under it, else its natural one. `None` for a credential JMAP cannot
    /// present at all.
    pub(crate) fn scheme_for(&self, credential: &Credential) -> Option<AuthScheme> {
        self.latched()
            .filter(|&scheme| can_present(credential, scheme))
            .or_else(|| natural_scheme(credential))
    }
}

/// The scheme `credential` is naturally presented under, before any server challenge is
/// seen; `None` for a kind JMAP cannot present (an administrator acting as another account:
/// HTTP has no authorization identity to carry it).
pub(crate) fn natural_scheme(credential: &Credential) -> Option<AuthScheme> {
    match credential {
        Credential::Password { .. } => Some(AuthScheme::Basic),
        Credential::Bearer { .. } => Some(AuthScheme::Bearer),
        _ => None,
    }
}

/// Whether `credential` can be presented under `scheme`.
///
/// A password can also be presented as a bearer token: the secret is the same opaque string
/// the user was issued (an API token or app password), and only the framing differs. Basic
/// wraps it with a username, Bearer sends it bare. The reverse does not hold: a token carries
/// no username to build a Basic header from, so it stays bearer-only.
pub(crate) fn can_present(credential: &Credential, scheme: AuthScheme) -> bool {
    match credential {
        Credential::Password { .. } => true,
        Credential::Bearer { .. } => scheme == AuthScheme::Bearer,
        _ => false,
    }
}

/// What a `401`'s challenge says about the credential presented under `used`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Challenge {
    /// The server offers another scheme the credential can be framed under: present it again
    /// that way. The credential was not judged.
    Reframe(AuthScheme),
    /// The server offers the scheme used, or sent no challenge at all: the credential itself
    /// was refused.
    Refused,
    /// The server offers only schemes this credential cannot be framed under. No replacement
    /// is asked for, deliberately: a token refused with a challenge for Basic alone would be
    /// replaced by another token, which the server refuses the same way, so every request
    /// would refresh the token against a server that never takes Bearer. A replacement of the
    /// same kind cannot answer it, so the `401` stands (`docs/agent-guidance/jmap.md`).
    Unusable,
}

/// Reads a `401`'s challenge headers for a credential presented under `used`.
///
/// When several offered schemes are presentable, Bearer wins: RFC 8620 §8.2 marks Basic
/// NOT RECOMMENDED, so it is the fallback rather than the preference. A `401` without a
/// challenge is a refusal: RFC 9110 §11.6.1 requires one, and a server that omits it has
/// still judged the credential (`spec-deviations.md`, YAHOO-1).
pub(crate) fn read_challenge<'a>(
    used: AuthScheme,
    challenges: impl IntoIterator<Item = &'a str>,
    credential: &Credential,
) -> Challenge {
    let offered: Vec<String> = challenges.into_iter().flat_map(challenge_schemes).collect();
    let offers = |scheme: AuthScheme| offered.iter().any(|s| s == scheme.token());
    if offered.is_empty() || offers(used) {
        return Challenge::Refused;
    }
    [AuthScheme::Bearer, AuthScheme::Basic]
        .into_iter()
        .find(|&scheme| offers(scheme) && can_present(credential, scheme))
        .map_or(Challenge::Unusable, Challenge::Reframe)
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
