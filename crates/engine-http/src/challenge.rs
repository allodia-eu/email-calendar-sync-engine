//! Reading the authentication schemes a server's challenge offers (RFC 9110 §11.6.1).
//!
//! A `401` names the schemes it accepts in `WWW-Authenticate`. Every adapter that presents a
//! credential over HTTP has to read them, to tell a refused credential from a refused framing or
//! a scheme it cannot answer at all, so the reading, the scheme a credential goes under, and the
//! rule for "this `401` asks for that scheme" live here once.

use engine_provider::Credential;
use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};

/// The lowercase scheme token a credential is naturally presented under: `basic` for a
/// password (RFC 7617), `bearer` for a token (RFC 6750). `None` for a kind HTTP cannot carry:
/// an administrator acting as another account has no authorization identity to send it in.
#[must_use]
pub fn credential_scheme(credential: &Credential) -> Option<&'static str> {
    match credential {
        Credential::Password { .. } => Some("basic"),
        Credential::Bearer { .. } => Some("bearer"),
        _ => None,
    }
}

/// The authentication schemes the `WWW-Authenticate` fields in `headers` offer, in order and as
/// the server spelled them. A response may carry several fields, which RFC 9110 §5.3 reads as
/// one list (Stalwart sends `Bearer` and `Basic` in two). A field that is not valid UTF-8 is
/// read lossily rather than dropped, so a realm with a non-ASCII name does not hide the schemes
/// beside it.
#[must_use]
pub fn offered_schemes(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .flat_map(|value| {
            challenge_schemes(&String::from_utf8_lossy(value.as_bytes()))
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether a `401` whose challenges offer `offered` asks for a credential under `scheme`: one
/// of them names it (case-insensitively), or there is no challenge at all.
///
/// RFC 9110 §15.5.2 says a `401` MUST carry a challenge, and some servers send none (YAHOO-1 in
/// `docs/spec-deviations.md`). A bare `401` has still judged the credential; a `401` whose
/// challenges all name other schemes has not, since that server has said what it wants.
#[must_use]
pub fn asks_for(offered: &[String], scheme: &str) -> bool {
    offered.is_empty()
        || offered
            .iter()
            .any(|token| token.eq_ignore_ascii_case(scheme))
}

/// The authentication schemes one `WWW-Authenticate` field value offers, in order and as the
/// server spelled them (schemes are case-insensitive, so compare with
/// [`str::eq_ignore_ascii_case`]).
///
/// The value is a comma-separated list of challenges, each a scheme token optionally followed by
/// a `token68` or by comma-separated `name=value` auth-params. The comma is therefore ambiguous:
/// it separates challenges *and* parameters. A segment is read as a new challenge only when its
/// first token is a bare word, since a `realm="…"` continuation carries an `=` and a scheme never
/// does. A comma inside a quoted parameter value (`realm="a,b"`, with `\"` escapes) separates
/// nothing.
///
/// A response may also carry several `WWW-Authenticate` fields (Stalwart sends `Bearer` and
/// `Basic` in two), and RFC 9110 §5.3 reads them as one list: call this on each.
#[must_use]
pub fn challenge_schemes(value: &str) -> Vec<&str> {
    unquoted_elements(value)
        .into_iter()
        .filter_map(|element| element.split_whitespace().next())
        .filter(|first| !first.contains('='))
        .collect()
}

/// `value` split at every comma outside a quoted-string (RFC 9110 §5.6.4).
fn unquoted_elements(value: &str) -> Vec<&str> {
    let (mut elements, mut start, mut quoted, mut escaped) = (Vec::new(), 0, false, false);
    for (at, ch) in value.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                elements.push(&value[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    elements.push(&value[start..]);
    elements
}

#[cfg(test)]
#[path = "challenge_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "challenge_header_tests.rs"]
mod header_tests;
