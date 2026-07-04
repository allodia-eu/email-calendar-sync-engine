//! The CalDAV request bodies this adapter sends (RFC 4791, RFC 6578, RFC 6764).
//!
//! All three are small, fixed XML documents (the `sync-collection` REPORT
//! interpolates the opaque sync-token). They request exactly the properties the
//! normalizers read, so a server returns nothing extra to parse.

/// `PROPFIND` (Depth 0) for the principal and its calendar home (RFC 6764 §6).
pub(crate) const PRINCIPAL_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
    r#"<d:prop><d:current-user-principal/><c:calendar-home-set/></d:prop></d:propfind>"#,
);

/// `PROPFIND` (Depth 1) listing a calendar home's collections and their metadata.
pub(crate) const CALENDAR_LIST_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" "#,
    r#"xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/">"#,
    r#"<d:prop><d:resourcetype/><d:displayname/><d:sync-token/><cs:getctag/>"#,
    r#"<ic:calendar-color/><c:calendar-description/></d:prop></d:propfind>"#,
);

/// Builds a `sync-collection` REPORT body (RFC 6578 §3.2) for the given prior
/// `sync_token` — empty for an initial (full) sync.
pub(crate) fn sync_collection_report(sync_token: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
            r#"<d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>"#,
            r#"<d:prop><d:getetag/><c:calendar-data/></d:prop></d:sync-collection>"#,
        ),
        token = xml_escape(sync_token),
    )
}

/// Escapes the five XML special characters so an opaque sync-token cannot break
/// out of its element.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_sync_sends_an_empty_token() {
        let body = sync_collection_report("");
        assert!(body.contains("<d:sync-token></d:sync-token>"));
        assert!(body.contains("<c:calendar-data/>"));
    }

    #[test]
    fn sync_token_is_escaped() {
        let body = sync_collection_report("a&b<c>\"d");
        assert!(body.contains("a&amp;b&lt;c&gt;&quot;d"));
        assert!(!body.contains("a&b<c>"));
    }
}
