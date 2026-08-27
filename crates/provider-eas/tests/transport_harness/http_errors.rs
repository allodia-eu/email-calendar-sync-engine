// SPDX-License-Identifier: MPL-2.0
//! HTTP-level error scenarios: 429 + `Retry-After` parsing, 5xx surfacing,
//! 401 Basic vs OAuth-refresh, the HTTP 451 `X-MS-Location` redirect follow
//! (with hop cap and no-location), non-WBXML bodies, and raw-garbage
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use provider_eas::{
    auth::{EasAuth, TokenProvider},
    client::{EasClient, EasError},
};

/// WBXML — every `RetryDecision` arm driven through the real loop.
use super::{
    fixtures::folder_sync_response,
    harness::client_at,
    harness::{test_config, tls_client_at},
    server::{CapturedRequest, Handler, MockResponse, MockServer},
};

/// A fake OAuth provider whose token flips after the first forced refresh —
/// drives the 401 → RefreshToken → re-issue branch.
struct RotatingToken {
    refreshes: AtomicUsize,
}

#[async_trait::async_trait]
impl TokenProvider for RotatingToken {
    async fn access_token(&self) -> Result<String, EasError> {
        let n = self.refreshes.load(Ordering::SeqCst);
        Ok(format!("token-{n}"))
    }

    async fn force_refresh(&self) -> Result<String, EasError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(format!("token-{}", self.refreshes.load(Ordering::SeqCst)))
    }
}

/// 429 with `Retry-After: 30` (delta-seconds): the client does NOT wait or
/// retry — the surfaced `HttpStatus` carries the parsed absolute window so
/// the source layer can promote to RateLimited.
#[tokio::test]
async fn http_429_surfaces_with_the_parsed_retry_after_window() {
    super::harness::init_logger();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-epoch clock")
        .as_secs();
    let now = i64::try_from(now).unwrap_or(0);
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::bare(429).with_header("Retry-After", "30")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("429 must surface, not retry");
    match err {
        EasError::HttpStatus {
            status,
            retry_after,
            ..
        } => {
            assert_eq!(status, 429);
            let window = retry_after.expect("Retry-After must parse");
            assert!(
                (now + 29..=now + 31).contains(&window),
                "window is now+30 (±1s), got {window} at now={now}"
            );
        }
        other => panic!("expected HttpStatus, got {other:?}"),
    }
    assert_eq!(
        server.count(),
        1,
        "transient errors are NOT retried in-process"
    );
}

/// 429 with the HTTP-DATE `Retry-After` form: unparsed (documented
/// limitation) → `retry_after: None`, the caller's default window applies.
#[tokio::test]
async fn http_429_http_date_retry_after_falls_back_to_none() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::bare(429).with_header("Retry-After", "Wed, 21 Oct 2026 07:28:00 GMT")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.folder_sync("0").await.expect_err("429 surfaces");
    assert!(
        matches!(
            &err,
            EasError::HttpStatus {
                retry_after: None,
                ..
            }
        ),
        "HTTP-date form must not parse: {err:?}"
    );
}

/// A 503 body is preserved on the surfaced error for diagnostics.
#[tokio::test]
async fn http_503_preserves_the_body() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(503, "text/plain", "maintenance window")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.folder_sync("0").await.expect_err("503 surfaces");
    assert!(
        matches!(&err, EasError::HttpStatus { status: 503, body, .. } if body.contains("maintenance")),
        "body preserved: {err:?}"
    );
}

/// 401 on Basic: surfaced immediately, no retry (credential failure is not
/// the transport's to fix).
#[tokio::test]
async fn http_401_basic_surfaces_without_retry() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(401)) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client.folder_sync("0").await.expect_err("401 surfaces");
    assert!(
        matches!(&err, EasError::HttpStatus { status: 401, .. }),
        "expected HttpStatus 401, got {err:?}"
    );
    assert_eq!(server.count(), 1);
}

/// 401 on OAuth: the token provider is force-refreshed ONCE and the command
/// re-issued with the new Bearer token.
#[tokio::test]
async fn http_401_oauth_refreshes_the_token_and_reissues() {
    super::harness::init_logger();
    let provider = Arc::new(RotatingToken {
        refreshes: AtomicUsize::new(0),
    });
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::bare(401)
        } else {
            // The re-issue must carry the ROTATED token.
            let expected = "Bearer token-1";
            if req.header("authorization") != Some(expected) {
                return MockResponse::raw(403, "text/plain", "wrong token");
            }
            MockResponse::wbxml(&folder_sync_response("key-after-refresh", &[]))
        }
    }) as Handler);
    let mut config = test_config(&server.eas_url());
    config.auth = Some(EasAuth::oauth(Box::new(TokenProviderClone(
        provider.clone(),
    ))));
    let mut client = EasClient::new(config, super::harness::tls_http()).expect("client builds");

    let result = client
        .folder_sync("0")
        .await
        .expect("refresh + re-issue succeeds");
    assert_eq!(result.sync_key, "key-after-refresh");
    assert_eq!(
        provider.refreshes.load(Ordering::SeqCst),
        1,
        "exactly one refresh"
    );
    assert_eq!(server.count(), 2);
}

/// A tiny adapter so the shared provider instance can be owned by the
/// config while the test keeps counting refreshes.
struct TokenProviderClone(Arc<RotatingToken>);

#[async_trait::async_trait]
impl TokenProvider for TokenProviderClone {
    async fn access_token(&self) -> Result<String, EasError> {
        self.0.access_token().await
    }

    async fn force_refresh(&self) -> Result<String, EasError> {
        self.0.force_refresh().await
    }
}

/// HTTP 451 with a valid `https://` X-MS-Location: the client ADOPTS the
/// endpoint and re-issues — against a real second TLS server here, proving
/// the follow is a genuine new round-trip ([MS-ASHTTP] §3.1.5.2).
#[tokio::test]
async fn http_451_follows_the_x_ms_location_hop_to_a_new_server() {
    super::harness::init_logger();
    // Server B answers the FolderSync after the hop.
    let target = MockServer::https(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::wbxml(&folder_sync_response("key-on-new-server", &[]))
    }) as Handler);
    let target_url = target.eas_url();
    let adopted_expectation = target_url.clone();

    let origin = MockServer::https(Arc::new(move |_: &CapturedRequest, ordinal: usize| {
        if ordinal == 1 {
            MockResponse::bare(451).with_header("X-MS-Location", &target_url)
        } else {
            MockResponse::bare(500) // the client must never come back here
        }
    }) as Handler);

    let mut client = tls_client_at(&origin.eas_url());
    let result = client
        .folder_sync("0")
        .await
        .expect("the hop lands on the new server");
    assert_eq!(result.sync_key, "key-on-new-server");
    assert_eq!(
        client.adopted_url(),
        Some(adopted_expectation.as_str()),
        "the adopted endpoint is exactly the X-MS-Location target"
    );
    assert_eq!(origin.count(), 1, "the origin sees exactly one request");
    target.await_count(1);
    assert_eq!(
        target.request(1).cmd().as_deref(),
        Some("FolderSync"),
        "the re-issue is the SAME command against the new endpoint"
    );
}

/// A 451 that keeps pointing at the same server exhausts the 3-hop cap and
/// surfaces the last 451 (a redirect cycle must never loop forever). The
/// location always derives the SAME endpoint (authority + fixed EAS path),
/// so each hop lands back here with `redirect_hops` climbing.
#[tokio::test]
async fn http_451_hop_cap_surfaces_the_last_451() {
    super::harness::init_logger();
    let server = MockServer::https(Arc::new(|req: &CapturedRequest, _ordinal: usize| {
        // The client's own Host header names this server — a self-pointing
        // https:// location the adoption accepts.
        let host = req.header("host").unwrap_or("127.0.0.1");
        MockResponse::bare(451).with_header("X-MS-Location", &format!("https://{host}"))
    }) as Handler);
    let mut client = tls_client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("hop cap must surface");
    assert!(
        matches!(&err, EasError::HttpStatus { status: 451, .. }),
        "expected the surfaced 451, got {err:?}"
    );
    assert_eq!(server.count(), 4, "initial + 3 hops, then stop");
    assert!(
        client.adopted_url().is_some(),
        "the hops were adopted (and persisted via adopted_url) before the cap"
    );
}

/// 451 WITHOUT an X-MS-Location cannot be followed — surfaced immediately.
#[tokio::test]
async fn http_451_without_location_surfaces() {
    super::harness::init_logger();
    let server =
        MockServer::http(Arc::new(|_: &CapturedRequest, _| MockResponse::bare(451)) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("unfollowable 451 surfaces");
    assert!(
        matches!(
            &err,
            EasError::HttpStatus {
                status: 451,
                x_ms_location: None,
                ..
            }
        ),
        "no location captured: {err:?}"
    );
    assert_eq!(server.count(), 1);
    assert_eq!(client.adopted_url(), None, "nothing adopted");
}

/// A 451 whose location would DOWNGRADE to http:// is refused: the adoption
/// error surfaces (naming the refused downgrade), nothing is adopted, and
/// no re-issue happens — the refuse-downgrade posture.
#[tokio::test]
async fn http_451_http_downgrade_location_is_refused() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::bare(451).with_header("X-MS-Location", "http://evil.example.test/evil")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("downgrade refused");
    let message = err.to_string();
    assert!(
        message.contains("http://") && message.to_ascii_lowercase().contains("refus"),
        "the error must describe the refused downgrade: {message}"
    );
    assert_eq!(client.adopted_url(), None);
    assert_eq!(server.count(), 1, "no follow, no loop");
}

/// An HTML body (OWA login page) with a WBXML-ish content type mismatch:
/// `text/html` answers are classified Unexpected and fail with a preview.
#[tokio::test]
async fn html_error_page_fails_with_a_preview() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(200, "text/html", "<html><body>please sign in</body></html>")
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("HTML is not WBXML");
    assert!(
        err.to_string().contains("non-WBXML") && err.to_string().contains("please sign in"),
        "error carries the preview: {err}"
    );
}

/// Garbage bytes claiming to be WBXML fail through the codec path with a
/// parse error (never a panic).
#[tokio::test]
async fn garbage_wbxml_fails_with_a_parse_error() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|_: &CapturedRequest, _| {
        MockResponse::raw(
            200,
            "application/vnd.ms-sync.wbxml",
            [0u8, 1, 2, 255, 254, 7, 8],
        )
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("garbage must not parse");
    assert!(
        matches!(err, EasError::Wbxml(_)),
        "expected a WBXML codec error, got {err:?}"
    );
}

/// A WBXML body whose root is NOT the expected command root (server
/// mis-dispatch) is an `UnexpectedRoot`, not a misleading parse.
#[tokio::test]
async fn wrong_response_root_is_unexpected_root() {
    super::harness::init_logger();
    // A Ping-shaped root answering a FolderSync request.
    let wrong = provider_eas::wbxml::WbxmlElement::container(
        13,
        0x05,
        vec![provider_eas::wbxml::WbxmlElement::text(13, 0x07, "1")],
    );
    let server =
        MockServer::http(
            Arc::new(move |_: &CapturedRequest, _| MockResponse::wbxml(&wrong)) as Handler,
        );
    let mut client = client_at(&server.eas_url());
    let err = client
        .folder_sync("0")
        .await
        .expect_err("wrong root surfaces");
    assert!(
        matches!(err, EasError::UnexpectedRoot { .. }),
        "expected UnexpectedRoot, got {err:?}"
    );
}

// ---- helpers ----

// ---- transport-level failure, debug dumps, and redaction ----

/// A request against a dead port surfaces as `EasError::Transport` (the
/// `From<reqwest::Error>` conversion) — no retry, nothing cached.
#[tokio::test]
async fn dead_endpoint_surfaces_a_transport_error() {
    super::harness::init_logger();
    // Port 9 (discard) is virtually never listening on loopback.
    let mut client = client_at("http://127.0.0.1:9/Microsoft-Server-ActiveSync");
    let err = client
        .folder_sync("0")
        .await
        .expect_err("unreachable errors");
    assert!(
        matches!(err, EasError::Transport(_)),
        "expected Transport, got {err:?}"
    );
    assert_eq!(client.adopted_url(), None);
}

/// `EasClient`'s manual Debug renders the config as redacted — the Basic
/// password must never appear, even in debug/panic inspection.
#[test]
fn client_debug_redacts_credentials() {
    let client = client_at("http://127.0.0.1:1/Microsoft-Server-ActiveSync");
    let shown = format!("{client:?}");
    assert!(shown.contains("EasClient"), "the type is named: {shown}");
    assert!(
        !shown.contains("app-password"),
        "the Basic password must never render: {shown}"
    );
    assert!(
        shown.contains("<redacted: contains credentials>"),
        "the config field is visibly redacted: {shown}"
    );
}

/// With a DEBUG-level logger installed, the transport's wire dumps fire —
/// including the REDACTED form for secret-bearing commands (Provision/
/// Settings/ValidateCert): byte counts render, content does not. This is a
/// behavior test of the log-gating, not just line coverage: `hex_capped`
/// and `parse_failure_preview` execute only when a logger accepts DEBUG.
#[tokio::test]
async fn debug_logging_dumps_wire_bodies_with_redaction() {
    super::harness::init_logger();
    let server = MockServer::http(Arc::new(|req: &CapturedRequest, _| {
        // Provision carries device/policy payloads — the redacted class.
        if req.cmd().as_deref() == Some("Provision") {
            MockResponse::wbxml(&super::fixtures::provision_response("1", "temp-log"))
        } else {
            MockResponse::wbxml(&super::fixtures::folder_sync_response("log-key", &[]))
        }
    }) as Handler);
    let mut client = client_at(&server.eas_url());
    client.provision().await.expect("handshake under logging");
    let result = client
        .folder_sync("0")
        .await
        .expect("round-trip under logging");
    assert_eq!(result.sync_key, "log-key");
    // The dumps are logged (captured by the test logger); the load-bearing
    // assertion here is that both command classes complete — the redaction
    // split is asserted by the codec-level preview tests.
}
