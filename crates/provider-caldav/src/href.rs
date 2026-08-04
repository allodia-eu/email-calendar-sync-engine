//! Href arithmetic: binding a calendar argument to a collection id, and the canonical
//! percent-encoding a minted resource href must use.
//!
//! Split out of `provider.rs` — these are pure string functions with no transport and no
//! provider state, and they are where two subtle CalDAV rules live: a collection href
//! always ends in a slash, and a minted href must be encoded the way the server
//! canonicalizes it or a later `If-Match`/`DELETE` misses the resource.

use engine_core::{
    calendar::Calendar,
    ids::{CalendarId, DavCollectionId},
};

use crate::error::CalDavError;

/// Binds a calendar argument to a collection id: an absolute path or full URL is
/// used as-is (a discovered calendar href), otherwise a bare name is joined onto
/// the calendar home. All end in a trailing slash (CalDAV collections are
/// directories).
///
/// # Errors
///
/// Returns [`CalDavError`] if the resolved href is not a valid provider key.
pub(crate) fn bind_collection(
    home_href: &str,
    calendar: &str,
) -> Result<DavCollectionId, CalDavError> {
    let href = resolve_collection(home_href, calendar);
    DavCollectionId::try_from(href.as_str())
        .map_err(|e| CalDavError::protocol(format!("bad collection href {href:?}: {e}")))
}

/// Adds a minimal [`Calendar`] for the bound collection when the home listing did
/// not include it, so the container snapshot always covers the events' membership.
pub(crate) fn ensure_bound_present(calendars: &mut Vec<Calendar>, bound: &CalendarId) {
    if calendars.iter().any(|c| &c.id == bound) {
        return;
    }
    let name = bound
        .as_str()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(bound.as_str())
        .to_owned();
    calendars.push(Calendar::new(bound.clone(), name));
}

/// Resolves the bound collection href (see [`bind_collection`]).
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
