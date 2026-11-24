//! Offline tests for the production Google transport: what each status becomes, and what the
//! anonymous byte path does *not* send.
//!
//! Its own file rather than an inline `mod tests`, like Graph's `http_transport_tests.rs`: the
//! transport and the cases that pin it had reached the 500-line limit together.

use std::io::{Read, Write};

use engine_core::error::FailureClass;

use super::*;

/// `token` as a source that never changes.
fn fixed(token: &str) -> Arc<dyn CredentialSource> {
    Arc::new(engine_provider::FixedCredential::unique(
        engine_provider::Credential::bearer(token),
    ))
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

/// A People `photos[].url` points off-origin (`googleusercontent.com`) and is served
/// publicly. The anonymous byte path exists so the OAuth token never travels there.
#[tokio::test]
async fn the_anonymous_byte_path_sends_no_authorization_header() {
    let body = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nbytes";
    let (base, seen) = mock_server_capturing(body.to_owned());
    let transport = HttpTransport::new(
        &base,
        fixed("super-secret"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();

    transport.get_bytes_unauthenticated(&base).await.unwrap();
    // The content host's connection is not the account's: what it negotiated is not recorded.
    assert_eq!(GoogleTransport::http_version(&transport), None);

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

fn http(status_line: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn get_parses_a_success_body() {
    let base = mock_server(http("200 OK", r#"{"labels":[]}"#));
    let transport = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    let doc = transport.get(&base).await.unwrap();
    assert!(doc.get("labels").is_some());
}

#[tokio::test]
async fn the_http_version_is_unknown_until_the_first_response() {
    // Google's connect performs no I/O (no session discovery), so a fresh transport
    // has observed nothing; the first response fills the fact in.
    let base = mock_server(http("200 OK", r#"{"labels":[]}"#));
    let transport = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap();
    assert_eq!(GoogleTransport::http_version(&transport), None);

    transport.get(&base).await.unwrap();
    assert_eq!(
        GoogleTransport::http_version(&transport),
        Some(HttpVersion::Http1_1)
    );
    // The mock server speaks cleartext, so there is no TLS version to report — and
    // reporting one anyway would be the tell that the fact is invented rather than
    // read off the connection. The real handshake is covered in `engine-http`.
    assert_eq!(GoogleTransport::tls_version(&transport), None);
}

#[tokio::test]
async fn non_success_status_becomes_a_classified_status_error() {
    let body = r#"{"error":{"code":401,"message":"nope","errors":[{"reason":"authError"}],"status":"UNAUTHENTICATED"}}"#;
    let base = mock_server(http("401 Unauthorized", body));
    let err = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .get(&base)
    .await
    .unwrap_err();
    assert!(matches!(&err, GoogleError::Status { reason: Some(r), .. } if r == "authError"));
    assert_eq!(err.failure_class(), FailureClass::Authentication);
}

#[tokio::test]
async fn a_non_json_success_body_is_a_permanent_decode_error() {
    let base = mock_server(http("200 OK", "this is not json"));
    let err = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .get(&base)
    .await
    .unwrap_err();
    // `Json`, not `Transport`: the bytes arrived fine and are not the JSON the
    // protocol requires, which is what that variant is for. It used to be `Transport`
    // only because `reqwest::Response::json` wrapped the parse failure as a decode
    // error of its own — and a reply now comes back through `engine_http::Sent`, whose
    // bytes this crate parses itself. The class callers branch on is unchanged.
    assert!(matches!(err, GoogleError::Json(_)));
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_refused_connection_is_a_retryable_transport_error() {
    let err = HttpTransport::new(
        "http://127.0.0.1:1",
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .get("http://127.0.0.1:1/gmail/v1/users/me/labels")
    .await
    .unwrap_err();
    assert!(matches!(err, GoogleError::Transport(_)));
    assert!(err.failure_class().is_retryable());
}

#[tokio::test]
async fn a_write_body_parses_json_or_treats_empty_as_none() {
    // A modify/send echoes JSON; a 204 (a trash/untrash no-body) carries none.
    let base = mock_server(http("200 OK", r#"{"id":"m1"}"#));
    let sent = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .post(&base, "application/json", b"{}".to_vec())
    .await
    .unwrap();
    assert_eq!(sent.unwrap()["id"], "m1");

    let base = mock_server(http("204 No Content", ""));
    HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .delete(&base, None)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_write_non_success_is_a_classified_status_error() {
    let body = r#"{"error":{"code":403,"message":"no","errors":[{"reason":"insufficientPermissions"}],"status":"PERMISSION_DENIED"}}"#;
    let base = mock_server(http("403 Forbidden", body));
    let err = HttpTransport::new(
        &base,
        fixed("tok"),
        crate::test_support::tls(),
        &crate::test_support::retry(),
        &crate::test_support::user_agent(),
    )
    .unwrap()
    .post(&base, "application/json", b"{}".to_vec())
    .await
    .unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}
