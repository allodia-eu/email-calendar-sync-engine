// SPDX-License-Identifier: MPL-2.0
// Autodiscover unit tests (flows are pure logic; the HTTP/DNS hops are
// live-tested in tests/live_eas.rs).

use super::{
    PoxOutcome, SrvRecordData, build_v1_pox_body, build_v2_url, parse_v1_pox_response,
    parse_v2_json_response, same_host, srv_autodiscover_url, srv_query_name,
};

#[test]
fn v1_pox_body_uses_mobilesync_request_schema() {
    // [MS-OXDISCO]: a mobilesync autodiscover request's document xmlns
    // must be the MOBILESYNC request schema — the outlook request schema
    // makes Exchange answer 600 Invalid Request (live-probed).
    let body = build_v1_pox_body("alice@example.com");
    assert!(body.contains(
            r#"xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/requestschema/2006""#
        ));
    assert!(body.contains(
        "http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006"
    ));
    assert!(body.contains("<EMailAddress>alice@example.com</EMailAddress>"));
    assert!(!body.contains("outlook/requestschema"));
}

#[test]
fn v2_url_declares_activesync_protocol() {
    // Without &Protocol=ActiveSync the V2 endpoint 400s
    // (Protocol_MissingProtocol — live-probed).
    let url = build_v2_url("alice@example.com");
    assert!(url.contains("Email=alice@example.com"));
    assert!(url.contains("Protocol=ActiveSync"));
}

#[test]
fn same_host_compares_url_hosts() {
    assert!(same_host(
        "https://mail.contoso.com/autodiscover/autodiscover.xml",
        "https://mail.contoso.com/other/path"
    ));
    assert!(!same_host(
        "https://mail.contoso.com/autodiscover/autodiscover.xml",
        "https://contoso.onmicrosoft.com/autodiscover/autodiscover.xml"
    ));
    // Unparseable URLs never match — fail closed (drop auth).
    assert!(!same_host("not a url", "https://mail.contoso.com/"));
}

#[test]
fn parse_v1_pox_extracts_server_url() {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response>
    <User><AutoDiscoverEmail>alice@example.com</AutoDiscoverEmail></User>
    <Action>Settings</Action>
    <MobileSync>
      <Server>
        <Type>MobileSync</Type>
        <Url>https://mail.contoso.com/Microsoft-Server-ActiveSync</Url>
        <Name>https://mail.contoso.com/Microsoft-Server-ActiveSync</Name>
      </Server>
    </MobileSync>
  </Response>
</Autodiscover>"#;
    let parsed = parse_v1_pox_response(body).unwrap();
    match parsed {
        PoxOutcome::Server(url) => {
            assert_eq!(url, "https://mail.contoso.com/Microsoft-Server-ActiveSync");
        }
        PoxOutcome::Redirect(_) => panic!("expected Server outcome"),
    }
}

#[test]
fn parse_v1_pox_returns_redirect_when_action_redirect() {
    let body = r#"<Autodiscover xmlns="...">
      <Response><Action>redirect</Action><Redirect><Url>https://contoso.onmicrosoft.com/autodiscover/autodiscover.xml</Url></Redirect></Response>
    </Autodiscover>"#;
    let parsed = parse_v1_pox_response(body).unwrap();
    match parsed {
        PoxOutcome::Redirect(url) => assert!(url.contains("contoso.onmicrosoft.com")),
        PoxOutcome::Server(_) => panic!("expected Redirect"),
    }
}

#[test]
fn parse_v2_json_extracts_url() {
    let body = r#"{"Url":"https://outlook.office365.com/Microsoft-Server-ActiveSync","Protocol":"ActiveSync"}"#;
    let url = parse_v2_json_response(body).unwrap();
    assert_eq!(
        url,
        "https://outlook.office365.com/Microsoft-Server-ActiveSync"
    );
}

#[test]
fn parse_v1_pox_rejects_error_response() {
    let body = r#"<Autodiscover xmlns="..."><Response><Error><ErrorCode>500</ErrorCode><Message>Invalid request</Message></Error></Response></Autodiscover>"#;
    assert!(parse_v1_pox_response(body).is_err());
}

// --- DNS SRV autodiscover fallback ([MS-ASCMD] §4.2 step 7) -------------
//
// `srv_autodiscover_url` record-selection truth table. The DNS query
// itself is NOT unit-testable (it needs a live resolver); the selection +
// URL-construction logic is pure and covered here.

fn srv(priority: u16, weight: u16, port: u16, target: &str) -> SrvRecordData {
    SrvRecordData {
        priority,
        weight,
        port,
        target: target.to_string(),
    }
}

#[test]
fn srv_empty_record_list_yields_none() {
    assert_eq!(srv_autodiscover_url(&[]), None);
}

#[test]
fn srv_lowest_priority_wins() {
    let records = [
        srv(10, 100, 443, "mail-a.contoso.com."),
        srv(5, 0, 443, "mail-b.contoso.com."),
    ];
    assert_eq!(
        srv_autodiscover_url(&records),
        Some("https://mail-b.contoso.com/autodiscover/autodiscover.xml".to_string())
    );
}

#[test]
fn srv_same_priority_highest_weight_wins() {
    let records = [
        srv(10, 10, 443, "mail-a.contoso.com."),
        srv(10, 50, 443, "mail-b.contoso.com."),
    ];
    assert_eq!(
        srv_autodiscover_url(&records),
        Some("https://mail-b.contoso.com/autodiscover/autodiscover.xml".to_string())
    );
}

#[test]
fn srv_full_tie_picks_first() {
    // Deterministic tie-break for tests: identical priority AND weight →
    // the first record in the list wins (real RFC 2782 weighting is
    // randomized; we pick determinism for reproducibility).
    let records = [
        srv(10, 10, 443, "mail-a.contoso.com."),
        srv(10, 10, 443, "mail-b.contoso.com."),
    ];
    assert_eq!(
        srv_autodiscover_url(&records),
        Some("https://mail-a.contoso.com/autodiscover/autodiscover.xml".to_string())
    );
}

#[test]
fn srv_port_443_omitted_from_url() {
    let records = [srv(0, 0, 443, "mail.contoso.com.")];
    assert_eq!(
        srv_autodiscover_url(&records),
        Some("https://mail.contoso.com/autodiscover/autodiscover.xml".to_string())
    );
}

#[test]
fn srv_custom_port_included_in_url() {
    let records = [srv(0, 0, 8443, "mail.contoso.com.")];
    assert_eq!(
        srv_autodiscover_url(&records),
        Some("https://mail.contoso.com:8443/autodiscover/autodiscover.xml".to_string())
    );
}

#[test]
fn srv_trailing_dot_on_target_stripped() {
    let records = [srv(0, 0, 443, "mail.contoso.com.")];
    let url = srv_autodiscover_url(&records).unwrap();
    assert!(!url.contains("com.."), "double dot leaked: {url}");
    assert!(url.starts_with("https://mail.contoso.com/"));
}

#[test]
fn srv_root_target_means_service_unavailable() {
    // RFC 2782: a target of "." means "decidedly not available".
    let records = [srv(0, 0, 443, ".")];
    assert_eq!(srv_autodiscover_url(&records), None);
}

#[test]
fn srv_query_name_is_autodiscover_tcp_domain() {
    assert_eq!(
        srv_query_name("contoso.com"),
        "_autodiscover._tcp.contoso.com."
    );
}
