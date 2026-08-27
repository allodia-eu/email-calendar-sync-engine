// SPDX-License-Identifier: MPL-2.0
//! Autodiscover scenarios ([MS-ASAUTOD]) over a real TLS mock: the V1 POX
//! Settings answer, the anonymous-first → 401 → Basic-upgrade exchange, and
//! the redirect profile (in-body POX `<Redirect>` and a cross-host HTTP 302
//! that must DROP the Authorization header).
//!
//! The flow hardcodes `https://<domain>/autodiscover/autodiscover.xml`
//! candidate URLs, so the mock TLS server is addressed through the EMAIL's
//! domain: `user@127.0.0.1:<port>` — the first candidate lands on the mock
//! and the flow returns before the V2 outlook.com fallback (which must
//! never be touched offline).
//!
//! The HTTP-302 profile runs with the test client's redirect policy set to
//! `none`, so the POX code's own redirect following (including the
//! cross-host credential drop) is what executes — reqwest's auto-follow
//! would otherwise satisfy the hop transparently and the manual branch
//! would never run.

use std::sync::Arc;

use provider_eas::autodiscover::autodiscover;

use super::{
    harness::tls_accept_any,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// A reqwest client on the harness TLS trust (accept-any), redirects
/// disabled so the POX flow's own redirect handling is under test.
fn no_redirect_client() -> reqwest::Client {
    tls_accept_any()
        .reqwest_builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("harness autodiscover client")
}

fn email_for(server: &MockServer) -> String {
    let base = server.base_url.trim_start_matches("https://");
    format!("user@{base}")
}

/// The plain V1 POX success: POST → `<MobileSync><Server><Url>` resolves.
#[tokio::test]
async fn autodiscover_v1_pox_resolves_the_eas_url() {
    super::harness::init_logger();
    let server = MockServer::https(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(
            200,
            "text/xml",
            pox_server_xml("https://mail.example.test/Microsoft-Server-ActiveSync"),
        )
    }) as Handler);
    let result = autodiscover(
        &email_for(&server),
        &no_redirect_client(),
        Some(("user@example.test", "app-password")),
    )
    .await
    .expect("V1 POX resolves");
    assert_eq!(
        result.eas_url,
        "https://mail.example.test/Microsoft-Server-ActiveSync"
    );
    // The candidate URL is the spec path, and the very first POST carries NO
    // Authorization (the credential is only offered after a 401 demands it).
    assert_eq!(
        server.request(1).path,
        "/autodiscover/autodiscover.xml",
        "the spec's V1 candidate path"
    );
    assert_eq!(
        server.request(1).header("authorization"),
        None,
        "V1 is anonymous-first — no credential leak"
    );
    // The request envelope is the mobilesync REQUEST schema and names the
    // mailbox ([MS-OXDISCO] — the outlook schema makes Exchange answer 600).
    let body = server.request(1).body_text();
    assert!(
        body.contains("mobilesync/requestschema/2006"),
        "document xmlns must be the mobilesync request schema: {body}"
    );
    assert!(
        body.contains(&format!(
            "<EMailAddress>{}</EMailAddress>",
            email_for(&server)
        )),
        "the envelope names the mailbox: {body}"
    );
}

/// 401 → Basic upgrade: the anonymous probe is refused, the SAME URL is
/// retried with the Basic header, and only then is the POX body accepted.
#[tokio::test]
async fn autodiscover_v1_upgrades_to_basic_after_401() {
    super::harness::init_logger();
    let server = MockServer::https(Arc::new(|_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::bare(401).with_header("WWW-Authenticate", "Basic realm=\"autodiscover\"")
        } else {
            MockResponse::raw(
                200,
                "text/xml",
                pox_server_xml("https://mail.example.test/Microsoft-Server-ActiveSync"),
            )
        }
    }) as Handler);
    let result = autodiscover(
        &email_for(&server),
        &no_redirect_client(),
        Some(("user@example.test", "app-password")),
    )
    .await
    .expect("upgrade path resolves");
    assert!(result.eas_url.contains("Microsoft-Server-ActiveSync"));
    let first = server.request(1);
    assert_eq!(first.header("authorization"), None, "probe is anonymous");
    let second = server.request(2);
    let auth = second.header("authorization").expect("Basic after 401");
    assert!(auth.starts_with("Basic "), "upgraded to Basic, got {auth}");
    assert_eq!(server.count(), 2);
}

/// The in-body POX redirect profile: `<Action>redirect</Action>` names the
/// next autodiscover URL (here a second path on the same mock); the flow
/// re-issues there and resolves.
#[tokio::test]
async fn autodiscover_pox_redirect_profile_follows_one_hop() {
    super::harness::init_logger();
    let server = MockServer::https(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            let host = req.header("host").unwrap_or("127.0.0.1");
            MockResponse::raw(
                200,
                "text/xml",
                pox_redirect_xml(&format!("https://{host}/autodiscover/hop2.xml")),
            )
        } else {
            MockResponse::raw(
                200,
                "text/xml",
                pox_server_xml("https://mail.example.test/Microsoft-Server-ActiveSync"),
            )
        }
    }) as Handler);
    let result = autodiscover(
        &email_for(&server),
        &no_redirect_client(),
        Some(("user@example.test", "app-password")),
    )
    .await
    .expect("redirect profile resolves");
    assert!(result.eas_url.contains("Microsoft-Server-ActiveSync"));
    assert_eq!(
        server.request(2).path,
        "/autodiscover/hop2.xml",
        "the re-issue landed on the redirected URL"
    );
    assert_eq!(server.count(), 2);
}

/// The cross-host HTTP 302 profile: after the Basic upgrade on the first
/// host, a 302 to a DIFFERENT host (`localhost` vs `127.0.0.1`) must drop
/// the Authorization header — credentials are never forwarded across hosts.
#[tokio::test]
async fn autodiscover_cross_host_302_drops_credentials() {
    super::harness::init_logger();
    let server = MockServer::https(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::bare(401)
        } else if ordinal == 2 {
            let port = req
                .header("host")
                .unwrap_or("127.0.0.1")
                .rsplit(':')
                .next()
                .unwrap_or_default();
            MockResponse::bare(302).with_header(
                "Location",
                &format!("https://localhost:{port}/autodiscover/autodiscover.xml"),
            )
        } else {
            MockResponse::raw(
                200,
                "text/xml",
                pox_server_xml("https://mail.example.test/Microsoft-Server-ActiveSync"),
            )
        }
    }) as Handler);
    let result = autodiscover(
        &email_for(&server),
        &no_redirect_client(),
        Some(("user@example.test", "app-password")),
    )
    .await
    .expect("cross-host hop resolves");
    assert!(result.eas_url.contains("Microsoft-Server-ActiveSync"));
    // Request 2 (on 127.0.0.1) carries Basic; request 3 (on localhost) must
    // carry NO Authorization — dropped on the host change.
    assert!(
        server.request(2).header("authorization").is_some(),
        "the upgraded Basic was sent on the original host"
    );
    let hop = server.request(3);
    assert_eq!(
        hop.header("authorization"),
        None,
        "credentials must be dropped across the host change"
    );
    assert!(
        hop.header("host")
            .unwrap_or_default()
            .starts_with("localhost"),
        "the hop really targeted the other host name"
    );
}

// ---- POX fixture bodies (local to the autodiscover scenarios) ----

/// A POX Settings response naming the EAS server URL.
fn pox_server_xml(eas_url: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
<Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/mobilesync/responseschema/2006">
<User><AutoDiscoverEmail>user@example.test</AutoDiscoverEmail></User>
<Action>Settings</Action>
<MobileSync><Server><Type>MobileSync</Type>
<Url>{eas_url}</Url><Name>{eas_url}</Name>
</Server></MobileSync>
</Response>
</Autodiscover>"#
    )
}

/// A POX redirect response: re-issue autodiscover at `target`.
fn pox_redirect_xml(target: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
<Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a">
<User><AutoDiscoverEmail>user@example.test</AutoDiscoverEmail></User>
<Action>redirect</Action>
<Redirect><Url>{target}</Url></Redirect>
</Response>
</Autodiscover>"#
    )
}
