// SPDX-License-Identifier: MPL-2.0
//! Session/OPTIONS scenarios ([MS-ASHTTP] §2.2.1.1): the HTTP OPTIONS
//! round-trip and version negotiation, over the real transport.

use std::sync::Arc;

use provider_eas::client::pick_protocol_version;

use super::{
    harness::client_at,
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// The server answers OPTIONS with both capability headers; the client
/// parses them and `pick_protocol_version` negotiates the newest known.
#[tokio::test]
async fn options_negotiates_protocol_version_from_capability_headers() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _ordinal: usize| {
        assert_eq!(req.method, "OPTIONS", "the client must send HTTP OPTIONS");
        MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "2.5,12.1,14.0,14.1,16.0,16.1")
            .with_header(
                "MS-ASProtocolCommands",
                "Sync,SendMail,Provision,FolderSync,Ping",
            )
    }) as Handler);
    let client = client_at(&server.eas_url());
    let options = client.options().await.expect("OPTIONS round-trip");
    assert_eq!(
        options.protocol_versions,
        vec!["2.5", "12.1", "14.0", "14.1", "16.0", "16.1"]
    );
    assert_eq!(options.commands.len(), 5);
    assert_eq!(
        pick_protocol_version("2.5,14.0,16.1", &["14.0", "16.1"]),
        Some("16.1".to_owned()),
        "negotiation picks the last client-known version"
    );

    // The request carried the Basic Authorization and User-Agent headers.
    let captured = server.request(1);
    let auth = captured.header("authorization").expect("Authorization");
    assert!(auth.starts_with("Basic "), "Basic auth header, got {auth}");
    assert_eq!(
        captured.header("user-agent"),
        Some("KylinsMail/1.0-harness")
    );
}

/// An OPTIONS response with NEITHER capability header is a Transport error —
/// the URL is almost certainly not an EAS endpoint (the unit-tested
/// `parse_options_headers` branch, driven through the real HTTP path).
#[tokio::test]
async fn options_without_capability_headers_is_a_transport_error() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(200)) as Handler);
    let client = client_at(&server.eas_url());
    let err = client.options().await.expect_err("no headers must error");
    assert!(
        err.to_string().contains("neither MS-ASProtocolVersions"),
        "error must name the missing headers: {err}"
    );
}

/// A non-2xx OPTIONS answer (an OWA login redirect / IIS error page) is
/// almost certainly not an EAS endpoint: `options()` surfaces the
/// descriptive Transport error — the status itself is not separately
/// classified (a documented wart: the capability-header check is the gate).
#[tokio::test]
async fn options_error_page_surfaces_a_transport_error() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(403, "text/html", "<html>forbidden</html>")
    }) as Handler);
    let client = client_at(&server.eas_url());
    let err = client.options().await.expect_err("not an EAS endpoint");
    assert!(
        err.to_string().contains("neither MS-ASProtocolVersions"),
        "the capability-header check is the gate: {err}"
    );
    assert_eq!(server.request(1).method, "OPTIONS");
}

/// OPTIONS on the typed `EasAuth::Basic` path (config.auth set — the same
/// header selection `send_command_no_retry` uses, through `options()`).
#[tokio::test]
async fn options_uses_the_typed_basic_auth_payload() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::bare(200)
            .with_header("MS-ASProtocolVersions", "16.1")
            .with_header("MS-ASProtocolCommands", "Sync")
    }) as Handler);
    let mut config = super::harness::test_config(&server.eas_url());
    config.auth = Some(provider_eas::auth::EasAuth::basic(
        "user@example.test",
        "app-password",
    ));
    let client =
        provider_eas::client::EasClient::new(config, super::harness::tls_http()).expect("build");
    let options = client.options().await.expect("OPTIONS round-trip");
    assert_eq!(options.protocol_versions, vec!["16.1"]);
    assert_eq!(
        server.request(1).header("authorization"),
        Some("Basic dXNlckBleGFtcGxlLnRlc3Q6YXBwLXBhc3N3b3Jk"),
        "the typed Basic payload builds the same header bytes"
    );
}
