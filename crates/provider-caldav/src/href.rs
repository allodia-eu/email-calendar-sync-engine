//! Href arithmetic: resolving a calendar argument to its collection href, and the canonical
//! percent-encoding a minted resource href must use.
//!
//! Split out of `provider.rs` — these are pure string functions with no transport and no
//! provider state, and they are where two subtle CalDAV rules live: a collection href
//! always ends in a slash, and a minted href must be encoded the way the server
//! canonicalizes it or a later `If-Match`/`DELETE` misses the resource.

use crate::error::CalDavError;

/// Resolves a discovery redirect's `Location` against the href that issued it.
///
/// DAV discovery walks in **href space**: most hrefs are server-issued paths that the
/// transport resolves onto the connection base, and a `Location` naming a bare path is
/// one of those, so it is passed along untouched. Once a hop has moved the chain to an
/// absolute URL, though, the next `Location` belongs to *that* origin. Leaving it a
/// path would resolve it onto the connection base instead, which after an origin change
/// is a different server: the request goes somewhere real, answers plausibly, and is
/// the wrong host.
///
/// An absolute `current` therefore resolves per RFC 9110 §10.2.2 (via
/// [`engine_provider::redirect_target`], which also refuses a hop that leaves TLS,
/// since these requests carry the account's credentials). `None` means the redirect
/// could not be resolved and the caller should fail rather than re-request `current`.
pub(crate) fn redirect_href(current: &str, location: &str) -> Option<String> {
    if current.contains("://") {
        return engine_provider::redirect_target(current, location);
    }
    Some(location.to_owned())
}

/// Resolves `reference`, an href a response carried, against `at`, the URL of the request
/// that returned it (RFC 4918 §8.3; RFC 3986 §5).
///
/// An absolute `reference` stands as it is. Against an absolute `at` a path resolves onto
/// `at`'s origin, which is how a principal named as `/123/principal/` by one host stays on that
/// host after discovery has moved on to another. Against a bare path (a replay fake, which has
/// no origin) a path stays a path.
///
/// # Errors
///
/// Returns [`CalDavError`] when `reference` cannot be resolved against `at`.
pub(crate) fn resolve_reference(at: &str, reference: &str) -> Result<String, CalDavError> {
    let Ok(base) = reqwest::Url::parse(at) else {
        return Ok(reference.to_owned());
    };
    if reqwest::Url::parse(reference).is_ok() {
        return Ok(reference.to_owned());
    }
    base.join(reference)
        .map(String::from)
        .map_err(|e| CalDavError::protocol(format!("bad href {reference:?}: {e}")))
}

/// Resolves a calendar argument to its collection href: an absolute path or full URL is
/// used as-is (a discovered calendar href), otherwise a bare name is joined onto the
/// calendar home. All end in a trailing slash (CalDAV collections are directories).
pub(crate) fn resolve_collection(home_href: &str, calendar: &str) -> String {
    if calendar.starts_with('/') || calendar.contains("://") {
        return with_trailing_slash(calendar);
    }
    format!(
        "{}{}/",
        with_trailing_slash(home_href),
        calendar.trim_matches('/')
    )
}

/// Ensures `href` ends with a single trailing slash.
fn with_trailing_slash(href: &str) -> String {
    if href.ends_with('/') {
        href.to_owned()
    } else {
        format!("{href}/")
    }
}

/// Percent-encodes one URL path segment to its **canonical** form: only RFC 3986
/// `unreserved` bytes (`ALPHA` / `DIGIT` / `-` / `.` / `_` / `~`) are kept verbatim;
/// every other byte — including `@`, sub-delims, and path-unsafe bytes — is
/// `%`-encoded. Encoding everything outside `unreserved` matches how CalDAV servers
/// store and report resource hrefs (Stalwart returns `@` as `%40`, verified live),
/// so a minted create href round-trips to the same href the server canonicalizes to
/// — otherwise a later `If-Match`/`DELETE` against the minted href would miss the
/// server's differently-encoded resource.
pub(crate) fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if is_unreserved(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_upper(byte >> 4));
            out.push(hex_upper(byte & 0x0f));
        }
    }
    out
}

/// Whether `byte` is an RFC 3986 `unreserved` character (never percent-encoded;
/// `%XX` and the literal byte are equivalent only for this set, §2.3).
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// The upper-case hex digit for a 0–15 nibble.
fn hex_upper(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('0', |c| c.to_ascii_uppercase())
}

/// `url` with every `%25` that is followed by two hex digits decoded once, so `%2540` becomes
/// `%40`; `None` when there is none.
///
/// Yahoo lists the events it names after their `UID` with the `%` of `%40` encoded again, then
/// refuses a request to that href with a `400` (`docs/spec-deviations.md`, YAHOO-8). The resource
/// answers once the extra layer is removed.
pub(crate) fn single_encoded(url: &reqwest::Url) -> Option<reqwest::Url> {
    let path = url.path();
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut changed = false;
    let mut at = 0;
    while at < bytes.len() {
        let doubled = bytes[at..].starts_with(b"%25")
            && bytes.get(at + 3).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(at + 4).is_some_and(u8::is_ascii_hexdigit);
        if doubled {
            out.push('%');
            at += 3;
            changed = true;
        } else {
            out.push(char::from(bytes[at]));
            at += 1;
        }
    }
    changed.then(|| {
        let mut once = url.clone();
        once.set_path(&out);
        once
    })
}
