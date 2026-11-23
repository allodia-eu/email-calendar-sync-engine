//! The CalDAV request bodies this adapter sends (RFC 4791, RFC 6578, RFC 6764).
//!
//! All three are small, fixed XML documents (the `sync-collection` REPORT
//! interpolates the opaque sync-token). They request exactly the properties the
//! normalizers read, so a server returns nothing extra to parse.

/// `PROPFIND` (Depth 0) for the principal, its calendar home (RFC 6764 §6) and the addresses
/// it schedules as (RFC 6638 §2.4.1).
///
/// It also asks for the principal's scheduling inbox and outbox (RFC 6638 §2.2): a principal
/// with neither, on a server that advertises no scheduling, has nothing to send an iTIP
/// message with, which is what lets an import into it be called silent.
pub(crate) const PRINCIPAL_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
    r#"<d:prop><d:current-user-principal/><c:calendar-home-set/>"#,
    r#"<c:calendar-user-address-set/><c:schedule-inbox-URL/><c:schedule-outbox-URL/>"#,
    r#"</d:prop></d:propfind>"#,
);

/// `PROPFIND` (Depth 0) of the user's own principal for the two routes to other principals'
/// calendar homes (`crate::shared`): the CalendarServer proxy sets, which name the principals
/// that delegated to this user, and RFC 3744 §5.8's `principal-collection-set`, the
/// collections that list principals.
pub(crate) const DELEGATION_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/">"#,
    r#"<d:prop><cs:calendar-proxy-read-for/><cs:calendar-proxy-write-for/>"#,
    r#"<d:principal-collection-set/></d:prop></d:propfind>"#,
);

/// `PROPFIND` describing principals (Depth 1 on a principal collection, Depth 0 on one
/// principal): what a store is called, where its calendars are, and whose address it is.
pub(crate) const PRINCIPAL_DESCRIPTION_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
    r#"<d:prop><d:displayname/><c:calendar-home-set/>"#,
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
    r#"<d:current-user-privilege-set/><d:owner/>"#,
    r#"<ic:calendar-color/><c:calendar-description/><c:supported-calendar-component-set/>"#,
    r#"</d:prop></d:propfind>"#,
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

/// Reads a calendar object's `schedule-tag` (RFC 6638 §3.2.10), for a delete guarded by one.
pub(crate) const SCHEDULE_TAG_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
    r#"<d:prop><c:schedule-tag/></d:prop></d:propfind>"#,
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

#[cfg(feature = "import")]
/// `PROPFIND` (Depth 0) of a calendar for what an import needs to know of it: the largest object
/// it takes (RFC 4791 §5.2.5), and its contents marker, which is the import cursor.
pub(crate) const CALENDAR_IMPORT_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" "#,
    r#"xmlns:cs="http://calendarserver.org/ns/">"#,
    r#"<d:prop><c:max-resource-size/><d:sync-token/><cs:getctag/></d:prop></d:propfind>"#,
);

/// An `MKCALENDAR` body (RFC 4791 §5.3.1) setting what a person sees of a new calendar: its
/// name, and its description and colour (the Apple `calendar-color` every surveyed server keeps)
/// where given.
pub(crate) fn mkcalendar(name: &str, description: Option<&str>, color: Option<&str>) -> String {
    let element = |tag: &str, value: &str| format!("<{tag}>{}</{tag}>", xml_escape(value));
    let mut body = String::from(concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" "#,
        r#"xmlns:ic="http://apple.com/ns/ical/"><d:set><d:prop>"#,
    ));
    body.push_str(&element("d:displayname", name));
    if let Some(description) = description {
        body.push_str(&element("c:calendar-description", description));
    }
    if let Some(color) = color {
        body.push_str(&element("ic:calendar-color", color));
    }
    body.push_str("</d:prop></d:set></c:mkcalendar>");
    body
}

#[cfg(feature = "import")]
/// A `calendar-query` REPORT (RFC 4791 §7.8) for the events whose `UID` contains `uid`, with
/// their data, so the caller keeps only the ones whose `UID` *is* `uid`: CalDAV's `text-match` is
/// a substring match (§9.7.5).
pub(crate) fn uid_query(uid: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">"#,
            r#"<d:prop><d:getetag/><c:calendar-data/></d:prop><c:filter>"#,
            r#"<c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT">"#,
            r#"<c:prop-filter name="UID"><c:text-match collation="i;octet">{uid}</c:text-match>"#,
            r#"</c:prop-filter></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#,
        ),
        uid = xml_escape(uid),
    )
}

#[cfg(feature = "import")]
/// `PROPFIND` (Depth 0) of an address book for what an import needs: the largest card it takes
/// (RFC 6352 §6.2.3), the vCard versions it takes (§6.2.2), and its contents marker.
pub(crate) const CONTACT_IMPORT_PROPFIND: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<d:propfind xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav" "#,
    r#"xmlns:cs="http://calendarserver.org/ns/"><d:prop><a:max-resource-size/>"#,
    r#"<a:supported-address-data/><d:sync-token/><cs:getctag/></d:prop></d:propfind>"#,
);

/// An extended `MKCOL` body (RFC 5689) making an address book with a name and, where given, a
/// description.
pub(crate) fn mkcol_address_book(name: &str, description: Option<&str>) -> String {
    let element = |tag: &str, value: &str| format!("<{tag}>{}</{tag}>", xml_escape(value));
    let mut body = String::from(concat!(
        r#"<?xml version="1.0" encoding="utf-8"?>"#,
        r#"<d:mkcol xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav"><d:set><d:prop>"#,
        r#"<d:resourcetype><d:collection/><a:addressbook/></d:resourcetype>"#,
    ));
    body.push_str(&element("d:displayname", name));
    if let Some(description) = description {
        body.push_str(&element("a:addressbook-description", description));
    }
    body.push_str("</d:prop></d:set></d:mkcol>");
    body
}

#[cfg(feature = "import")]
/// An `addressbook-query` REPORT (RFC 6352 §8.6) for the card whose `UID` equals `uid`
/// (`match-type="equals"`, §10.5.4), with its data so the caller can check the match itself.
pub(crate) fn card_uid_query(uid: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<a:addressbook-query xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
            r#"<d:prop><d:getetag/><a:address-data/></d:prop><a:filter>"#,
            r#"<a:prop-filter name="UID"><a:text-match collation="i;octet" match-type="equals">"#,
            r#"{uid}</a:text-match></a:prop-filter></a:filter></a:addressbook-query>"#,
        ),
        uid = xml_escape(uid),
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
pub(crate) fn xml_escape(value: &str) -> String {
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
