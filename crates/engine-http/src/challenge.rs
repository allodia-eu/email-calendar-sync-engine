//! Reading the authentication schemes a server's challenge offers (RFC 9110 §11.6.1).
//!
//! A `401` names the schemes it accepts in `WWW-Authenticate`. Every adapter that presents a
//! credential over HTTP has to read them, to tell a refused credential from a refused framing or
//! a scheme it cannot answer at all, so the parsing lives here once.

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
