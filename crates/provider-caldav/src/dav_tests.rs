//! Tests for the WebDAV `multistatus` parser ([`super`]), split out to keep
//! `dav.rs` under the file-length limit.

use super::*;

#[test]
fn parses_calendar_home_listing_skipping_404_props() {
    let xml = include_str!("../tests/fixtures/calendar-home.xml");
    let parsed = parse_multistatus(xml).unwrap();
    // The home itself (a plain collection) plus the default calendar.
    assert_eq!(parsed.responses.len(), 2);
    let calendar = parsed
        .responses
        .iter()
        .find(|r| r.props.is_calendar())
        .expect("the default calendar is a CalDAV calendar");
    assert_eq!(calendar.href(), "/dav/cal/alice%40test.local/default/");
    assert_eq!(
        calendar.props.get("displayname"),
        Some("Stalwart Calendar (alice@test.local)")
    );
    // The CTag came back; the unsupported calendar-color was a 404 propstat
    // and must not leak into the props.
    assert_eq!(calendar.props.get("getctag"), Some("\"22\""));
    assert_eq!(calendar.props.get("calendar-color"), None);
    // The home href is a collection but not a calendar.
    let home = parsed
        .responses
        .iter()
        .find(|r| !r.props.is_calendar())
        .unwrap();
    assert!(!home.props.is_calendar());
}

#[test]
fn collects_the_privileges_nested_inside_current_user_privilege_set() {
    let xml = include_str!("../tests/fixtures/calendar-home.xml");
    let calendar = parse_multistatus(xml)
        .unwrap()
        .responses
        .into_iter()
        .find(|r| r.props.is_calendar())
        .unwrap();
    let privileges = calendar.props.privileges().expect("Stalwart reports them");
    // Each privilege is an empty element two levels down (`<privilege><write/>`),
    // so it carries no text at all — the leaf-text path cannot see it.
    assert!(privileges.contains("write"));
    assert!(privileges.contains("write-content"));
    assert!(privileges.contains("bind"));
    // Prefix-agnostic: `read-free-busy` is in the CalDAV namespace, the rest in DAV:.
    assert!(privileges.contains("read-free-busy"));
}

#[test]
fn a_read_only_share_reports_privileges_without_a_write() {
    let xml = include_str!("../tests/fixtures/calendar-home-sabredav.xml");
    let shared = parse_multistatus(xml)
        .unwrap()
        .responses
        .into_iter()
        .find(|r| r.href().ends_with("/bob-readonly/"))
        .expect("the read-only share is listed");
    let privileges = shared.props.privileges().expect("SabreDAV reports them");
    assert!(privileges.contains("read"));
    // It grants `write-properties` — renaming the collection — but no privilege
    // that would let an event be written into it.
    assert!(privileges.contains("write-properties"));
    assert!(!privileges.contains("write"));
    assert!(!privileges.contains("write-content"));
    assert!(!privileges.contains("all"));
}

#[test]
fn a_silent_server_reports_no_privilege_set_while_an_empty_one_grants_nothing() {
    // Not asked / not answered: the property is absent from the propstat entirely.
    let silent = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/c/</D:href><D:propstat><D:prop><D:displayname>C</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    assert_eq!(
        parse_multistatus(silent).unwrap().responses[0]
            .props
            .privileges(),
        None
    );

    // Answered, with nothing in it: "you may do nothing here" — a set, not silence.
    let empty = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/c/</D:href><D:propstat><D:prop><D:current-user-privilege-set/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    assert!(
        parse_multistatus(empty).unwrap().responses[0]
            .props
            .privileges()
            .expect("the property was reported")
            .is_empty()
    );
}

#[test]
fn a_404_propstat_privilege_set_does_not_leak_in() {
    // A server that lists the unsupported property in its `404` propstat must not
    // be read as "reported an empty privilege set" — that would flip every
    // collection to read-only.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/c/</D:href><D:propstat><D:prop><D:current-user-privilege-set/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat><D:propstat><D:prop><D:displayname>C</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    assert_eq!(
        parse_multistatus(xml).unwrap().responses[0]
            .props
            .privileges(),
        None
    );
}

#[test]
fn parses_principal_and_home_hrefs() {
    let xml = include_str!("../tests/fixtures/principal.xml");
    let parsed = parse_multistatus(xml).unwrap();
    let response = &parsed.responses[0];
    assert_eq!(
        response.props.get("current-user-principal"),
        Some("/dav/pal/alice%40test.local/")
    );
    assert_eq!(
        response.props.get("calendar-home-set"),
        Some("/dav/cal/alice%40test.local/")
    );
}

#[test]
fn unescapes_entity_escaped_property_text() {
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:displayname>Alice &amp; Bob &lt;Team&gt;</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let parsed = parse_multistatus(xml).unwrap();
    assert_eq!(
        parsed.responses[0].props.get("displayname"),
        Some("Alice & Bob <Team>")
    );
}

#[test]
fn resolves_numeric_and_hex_character_references_in_property_text() {
    // A server may spell a character as a numeric reference rather than a named
    // entity — `&#38;` / `&#x26;` for `&`, and `&#13;` for the CR that XML
    // end-of-line normalization would otherwise eat out of a folded value.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:displayname>R&#38;D&#x26;More&#13;&#10;next</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let parsed = parse_multistatus(xml).unwrap();
    assert_eq!(
        parsed.responses[0].props.get("displayname"),
        Some("R&D&More\r\nnext")
    );
}

#[test]
fn an_out_of_range_character_reference_is_an_error_not_a_panic() {
    // `&#x110000;` is past the last Unicode scalar. Hostile input must come back
    // as a classified error, never a panic — the whole point of this parser.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/&#x110000;/</D:href></D:response></D:multistatus>";
    assert!(parse_multistatus(xml).is_err());
}

#[test]
fn an_undeclared_entity_is_an_error_not_a_silent_hole() {
    // A `multistatus` carries no DTD, so only the five predefined entities are
    // declared. Dropping an unknown one would hand a *truncated* value to the
    // caller — a shortened href or a mangled iCalendar — which is worse than
    // refusing the document.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/&nbsp;/</D:href><D:propstat><D:prop><D:displayname>x</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let err = parse_multistatus(xml).unwrap_err();
    assert!(
        err.to_string().contains("&nbsp;"),
        "the error should name the entity it could not resolve: {err}"
    );
}

#[test]
fn parses_sync_collection_with_etags_and_cdata_calendar_data() {
    let xml = include_str!("../tests/fixtures/sync-initial.xml");
    let parsed = parse_multistatus(xml).unwrap();
    assert_eq!(
        parsed.sync_token.as_deref(),
        Some("urn:stalwart:davsync:16")
    );
    // The collection self-response (no calendar-data) plus six resources.
    let resources: Vec<_> = parsed
        .responses
        .iter()
        .filter(|r| r.props.get("calendar-data").is_some())
        .collect();
    assert_eq!(resources.len(), 6);
    let oneoff = resources
        .iter()
        .find(|r| r.href().ends_with("oneoff-2001.ics"))
        .unwrap();
    assert!(oneoff.props.get("getetag").is_some());
    // The CDATA iCalendar survived intact.
    let data = oneoff.props.get("calendar-data").unwrap();
    assert!(data.contains("UID:oneoff-2001@test.local"));
    assert!(data.contains("BEGIN:VEVENT"));
}

#[test]
fn parses_noop_delta_token_with_no_responses() {
    let xml = include_str!("../tests/fixtures/sync-noop.xml");
    let parsed = parse_multistatus(xml).unwrap();
    assert!(parsed.responses.is_empty());
    assert_eq!(
        parsed.sync_token.as_deref(),
        Some("urn:stalwart:davsync:16")
    );
}

#[test]
fn recognizes_a_removal_response() {
    // A sync-collection delta reports a deleted resource as a 404 response.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/gone.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>t2</D:sync-token></D:multistatus>";
    let parsed = parse_multistatus(xml).unwrap();
    assert_eq!(parsed.responses.len(), 1);
    assert!(parsed.responses[0].is_removed());
    assert_eq!(parsed.responses[0].href(), "/cal/gone.ics");
}

#[test]
fn captures_a_property_value_nested_below_a_single_href() {
    // A server that wraps current-user-principal deeper than a direct <href>
    // must still yield the href (else discovery fails to find the principal).
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:authenticated-as><D:href>/principals/u/</D:href></D:authenticated-as></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    let parsed = parse_multistatus(xml).unwrap();
    assert_eq!(
        parsed.responses[0].props.get("current-user-principal"),
        Some("/principals/u/")
    );
}

#[test]
fn keeps_every_href_in_a_multi_href_response() {
    // RFC 4918 §14.16: a status-only response may cover several hrefs; a
    // multi-href removal must tombstone all of them, not just the first.
    let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/a.ics</D:href><D:href>/b.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>t2</D:sync-token></D:multistatus>";
    let parsed = parse_multistatus(xml).unwrap();
    assert!(parsed.responses[0].is_removed());
    assert_eq!(parsed.responses[0].hrefs, vec!["/a.ics", "/b.ics"]);
    assert_eq!(parsed.responses[0].href(), "/a.ics");
}

#[test]
fn malformed_xml_is_an_error_not_a_panic() {
    assert!(parse_multistatus("<D:multistatus><unclosed>").is_err());
}
