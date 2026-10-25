//! The CalDAV request bodies this adapter sends (RFC 4791, RFC 6578, RFC 6764).
//!
//! All three are small, fixed XML documents (the `sync-collection` REPORT
//! interpolates the opaque sync-token). They request exactly the properties the
//! normalizers read, so a server returns nothing extra to parse.

/// `PROPFIND` (Depth 0) for the principal, its calendar home (RFC 6764 §6) and the addresses
/// it schedules as (RFC 6638 §2.4.1).
pub(crate) const PRINCIPAL_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
    r#"<d:prop><d:current-user-principal/><c:calendar-home-set/>"#,
    r#"<c:calendar-user-address-set/></d:prop></d:propfind>"#,
);

/// `PROPFIND` (Depth 1) listing a calendar home's collections and their metadata.
///
/// `current-user-privilege-set` (RFC 3744 §5.4) is what the *authenticated* principal
/// may do **here** — the only honest source for
/// [`CalendarAccess`][engine_core::calendar::CalendarAccess], since a privilege is granted per
/// principal, not per collection. It rides this PROPFIND rather than a second round trip per
/// collection.
pub(crate) const CALENDAR_LIST_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" "#,
    r#"xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/">"#,
    r#"<d:prop><d:resourcetype/><d:displayname/><d:sync-token/><cs:getctag/>"#,
    r#"<d:current-user-privilege-set/>"#,
    r#"<ic:calendar-color/><c:calendar-description/></d:prop></d:propfind>"#,
);

/// CardDAV principal/address-book-home discovery. Both properties go in one request: Yahoo
/// answers a principal `PROPFIND` for `current-user-principal` alone with a `400`
/// (`docs/spec-deviations.md`, YAHOO-5).
pub(crate) const ADDRESS_BOOK_PRINCIPAL_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
    r#"<d:prop><d:current-user-principal/><a:addressbook-home-set/></d:prop></d:propfind>"#,
);

/// Lists address-book collections and rights.
pub(crate) const ADDRESS_BOOK_LIST_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav" "#,
    r#"xmlns:cs="http://calendarserver.org/ns/">"#,
    r#"<d:prop><d:resourcetype/><d:displayname/><d:sync-token/><cs:getctag/>"#,
    r#"<d:current-user-privilege-set/><a:addressbook-description/></d:prop></d:propfind>"#,
);

/// What a calendar sync and a calendar multiget ask for of each resource: one string, so a body
/// a multiget fills in carries exactly what the sync would have.
const CALENDAR_OBJECT_PROPS: &str =
    "<d:prop><d:getetag/><c:schedule-tag/><c:calendar-data/></d:prop>";

/// What an address-book sync and an address-book multiget ask for of each card.
const ADDRESS_OBJECT_PROPS: &str =
    r#"<d:prop><d:getetag/><a:address-data content-type="text/vcard" version="4.0"/></d:prop>"#;

/// CardDAV RFC 6578 sync report requesting vCard payloads.
pub(crate) fn address_book_sync_report(sync_token: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<d:sync-collection xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
            r#"<d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>"#,
            "{props}</d:sync-collection>",
        ),
        token = xml_escape(sync_token),
        props = ADDRESS_OBJECT_PROPS,
    )
}

/// Reads a collection's CTag when RFC 6578 sync is unavailable.
pub(crate) const CTAG_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">"#,
    r#"<d:prop><cs:getctag/></d:prop></d:propfind>"#,
);

/// Lists a collection's members with their ETags, for the CTag fallback to fetch by multiget.
pub(crate) const MEMBER_ETAGS_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>"#,
);

/// Asks a resource who the requester is (RFC 5397): the cheapest request whose answer shows
/// whether the server knows, and so draws the challenge from one that wants the credential.
pub(crate) const CURRENT_USER_PRINCIPAL_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#,
);

/// Builds a `sync-collection` REPORT body (RFC 6578 §3.2) for the given prior
/// `sync_token` — empty for an initial (full) sync.
pub(crate) fn sync_collection_report(sync_token: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
            r#"<d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>"#,
            "{props}</d:sync-collection>",
        ),
        token = xml_escape(sync_token),
        props = CALENDAR_OBJECT_PROPS,
    )
}

/// A `calendar-multiget` REPORT (RFC 4791 §7.9) for `hrefs`, asking for what
/// [`sync_collection_report`] asks for.
pub(crate) fn calendar_multiget_report(hrefs: &[String]) -> String {
    multiget(
        r#"<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
        CALENDAR_OBJECT_PROPS,
        "</c:calendar-multiget>",
        hrefs,
    )
}

/// An `addressbook-multiget` REPORT (RFC 6352 §8.7) for `hrefs`, asking for what
/// [`address_book_sync_report`] asks for.
pub(crate) fn addressbook_multiget_report(hrefs: &[String]) -> String {
    multiget(
        r#"<a:addressbook-multiget xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
        ADDRESS_OBJECT_PROPS,
        "</a:addressbook-multiget>",
        hrefs,
    )
}

/// An `addressbook-multiget` REPORT for `hrefs` that asks for each card as the server stores it,
/// with no `version`: the CTag fallback and a single card's refetch ask for nothing the server
/// was not already asked for, and RFC 6352 requires a server to support vCard 3.0 only, so a
/// server that cannot convert to 4.0 may refuse a request for it.
pub(crate) fn addressbook_multiget_as_stored(hrefs: &[String]) -> String {
    multiget(
        r#"<a:addressbook-multiget xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
        "<d:prop><d:getetag/><a:address-data/></d:prop>",
        "</a:addressbook-multiget>",
        hrefs,
    )
}

fn multiget(open: &str, prop: &str, close: &str, hrefs: &[String]) -> String {
    let mut body = format!(r#"<?xml version="1.0" encoding="utf-8"?>{open}{prop}"#);
    for href in hrefs {
        body.push_str("<d:href>");
        body.push_str(&xml_escape(href));
        body.push_str("</d:href>");
    }
    body.push_str(close);
    body
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
        assert!(body.contains("<c:schedule-tag/>"));
        assert!(body.contains("<c:calendar-data/>"));
    }

    #[test]
    fn sync_token_is_escaped() {
        let body = sync_collection_report("a&b<c>\"d");
        assert!(body.contains("a&amp;b&lt;c&gt;&quot;d"));
        assert!(!body.contains("a&b<c>"));
    }
}
