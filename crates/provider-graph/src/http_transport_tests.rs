//! Offline tests for the production Graph transport: what each status becomes, what the
//! anonymous byte path does *not* send, and the throttle a real mailbox answers with.
//!
//! Its own file rather than an inline `mod tests`, like `fetch_tests.rs` beside `fetch.rs`:
//! the transport and the cases that pin it had grown past the 500-line limit together.

use std::io::{Read, Write};

use engine_core::error::FailureClass;

use super::*;

/// A blocking single-shot mock HTTP server: serves `response` to one connection, so
/// the live reqwest transport runs offline (no network).
fn mock_server(response: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

/// Serves one canned response and hands back the raw request head, so a test can
/// assert on the headers that actually went out.
fn mock_server_capturing(response: String) -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&seen);
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            *sink.lock().unwrap() = String::from_utf8_lossy(&buf[..read]).into_owned();
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

/// The bytes a live Microsoft 365 mailbox actually refuses with, replayed.
///
/// Captured whole (`tests/fixtures/README.md`), because every part of it is load-bearing
/// and none of it is guessable: the status, the `Retry-After` the server names, and the
/// body that says *which* ceiling was hit. An invented `429` with an empty body — which
/// is what `error.rs` had — proves the status mapping and nothing about the refusal a
/// server sends.
///
/// What this pins is that the refusal is **rate-limited, not permanent**. A body dropped
/// as permanent is one no later pass fetches again, so reading `ApplicationThrottled` as
/// a failure rather than a wait loses mail rather than delaying it.
#[tokio::test]
async fn the_throttle_a_real_mailbox_sends_is_a_wait_and_not_a_loss() {
    const THROTTLED: &str =
        include_str!("../tests/fixtures/mail/throttled_mailbox_concurrency.json");
    // The captured response, `Retry-After` included. `RetryPolicy::none()` keeps the
    // test about the classification: waiting out seven seconds is `engine-http`'s
    // business and `send_tests.rs` already asserts it.
    let response = format!(
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\n\
         Retry-After: 7\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{THROTTLED}",
        THROTTLED.len()
    );
    let base = mock_server(response);
    let retry = engine_http::RetryConfig::default().with_policy(engine_http::RetryPolicy::none());
    let transport =
        HttpTransport::new("tok".to_owned(), crate::test_support::tls(), &retry).unwrap();

    let err = transport.get(&base).await.expect_err("a refusal");

    assert_eq!(
        err.failure_class(),
        FailureClass::RateLimited,
        "a concurrency refusal read as anything else is mail this pass drops",
    );
    // And the reason survives into the error, so a diagnostic log can say which ceiling
    // was hit rather than only that something was refused.
    let rendered = err.to_string();
    assert!(
        rendered.contains("MailboxConcurrency"),
        "the refusal lost what Graph said about it: {rendered}",
    );
}

/// A contact payload can carry a photo URI naming any host. The anonymous byte
/// path exists so the account's OAuth token never reaches such a host.
#[tokio::test]
async fn the_anonymous_byte_path_sends_no_authorization_header() {
    let (base, seen) = mock_server_capturing(http("200 OK", "bytes"));
    let transport = HttpTransport::new(
        "super-secret".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap();

    transport.get_bytes_unauthenticated(&base).await.unwrap();

    let sent = seen.lock().unwrap().to_lowercase();
    assert!(
        !sent.contains("authorization"),
        "OAuth token leaked to a payload-named host: {sent}"
    );
    assert!(
        !sent.contains("super-secret"),
        "token in the request: {sent}"
    );
}

fn http(status_line: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn get_parses_a_success_body() {
    let base = mock_server(http("200 OK", r#"{"value":[]}"#));
    let transport = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap();
    let doc = transport.get(&base).await.unwrap();
    assert!(doc.get("value").is_some());
}

#[tokio::test]
async fn the_http_version_is_unknown_until_the_first_response() {
    // Graph's connect performs no I/O (no session discovery), so a fresh transport
    // has observed nothing; the first response fills the fact in. This is the one
    // provider where `ConnectionInfo::http_version` is `None` on a live connection.
    let base = mock_server(http("200 OK", r#"{"value":[]}"#));
    let transport = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap();
    assert_eq!(GraphTransport::http_version(&transport), None);

    transport.get(&base).await.unwrap();
    assert_eq!(
        GraphTransport::http_version(&transport),
        Some(HttpVersion::Http1_1)
    );
    // The mock server speaks cleartext, so there is no TLS version to report — and
    // reporting one anyway would be the tell that the fact is invented rather than
    // read off the connection. The real handshake is covered in `engine-http`.
    assert_eq!(GraphTransport::tls_version(&transport), None);
}

#[tokio::test]
async fn get_bytes_returns_the_raw_body_verbatim() {
    // `$value` is not JSON — it is the raw RFC 822 MIME; get_bytes returns it as-is.
    let mime = "From: a@example.com\r\nSubject: Hi\r\n\r\nBody\r\n";
    let base = mock_server(http("200 OK", mime));
    let bytes = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .get_bytes(&base)
    .await
    .unwrap();
    assert_eq!(bytes, mime.as_bytes());
}

#[tokio::test]
async fn get_bytes_non_success_is_a_classified_status_error() {
    // A gone/moved message → the same status classification as any Graph error.
    let body = r#"{"error":{"code":"ErrorItemNotFound","message":"gone"}}"#;
    let base = mock_server(http("404 Not Found", body));
    let err = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .get_bytes(&base)
    .await
    .unwrap_err();
    assert!(matches!(&err, GraphError::Status { code: Some(c), .. } if c == "ErrorItemNotFound"));
}

#[tokio::test]
async fn non_success_status_becomes_a_classified_status_error() {
    let body = r#"{"error":{"code":"InvalidAuthenticationToken","message":"nope"}}"#;
    let base = mock_server(http("401 Unauthorized", body));
    let err = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .get(&base)
    .await
    .unwrap_err();
    assert!(
        matches!(&err, GraphError::Status { code: Some(c), .. } if c == "InvalidAuthenticationToken")
    );
    assert_eq!(err.failure_class(), FailureClass::Authentication);
}

#[tokio::test]
async fn a_non_json_success_body_is_a_permanent_decode_error() {
    let base = mock_server(http("200 OK", "this is not json"));
    let err = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .get(&base)
    .await
    .unwrap_err();
    // A body that does not decode is a permanent protocol mismatch.
    assert!(matches!(err, GraphError::Transport(_)));
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_refused_connection_is_a_retryable_transport_error() {
    // Nothing is listening on this port → reqwest connect error → retryable.
    let err = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .get("http://127.0.0.1:1/me")
    .await
    .unwrap_err();
    assert!(matches!(err, GraphError::Transport(_)));
    assert!(err.failure_class().is_retryable());
}

#[tokio::test]
async fn a_write_body_parses_json_or_treats_empty_as_none() {
    // A create/patch echoes JSON; a 202/204 carries none.
    let base = mock_server(http("201 Created", r#"{"id":"x"}"#));
    let created = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .post(&base, "application/json", b"{}".to_vec())
    .await
    .unwrap();
    assert_eq!(created.unwrap()["id"], "x");

    let base = mock_server(http("204 No Content", ""));
    let none = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .patch(&base, "application/json", Some("etag"), b"{}".to_vec())
    .await
    .unwrap();
    assert!(none.is_none());
}

#[tokio::test]
async fn a_412_write_is_a_classified_conflict() {
    let body = r#"{"error":{"code":"ErrorIrresolvableConflict","message":"stale"}}"#;
    let base = mock_server(http("412 Precondition Failed", body));
    let err = HttpTransport::new(
        "tok".to_owned(),
        crate::test_support::tls(),
        &crate::test_support::retry(),
    )
    .unwrap()
    .delete(&base, Some("stale-etag"))
    .await
    .unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Conflict);
}
