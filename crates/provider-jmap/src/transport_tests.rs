//! Wire-level tests for challenge-driven authentication negotiation.
//!
//! [`crate::auth`] unit-tests the parsing and the policy in isolation; these prove the
//! transport actually re-frames and replays the request on a real socket — that the
//! second attempt goes out with the scheme the server asked for, carrying the same body,
//! and that the switch is latched rather than re-negotiated on every call.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex, OnceLock},
};

use engine_tls::TlsClientConfig;
use serde_json::json;

use super::*;

/// A throwaway trust policy; these tests run over plaintext, so it is never exercised.
fn tls() -> &'static TlsClientConfig {
    static TLS: OnceLock<TlsClientConfig> = OnceLock::new();
    TLS.get_or_init(TlsClientConfig::bundled)
}

/// One request as the mock server saw it.
#[derive(Debug, Clone)]
struct Seen {
    authorization: String,
    body: String,
}

/// A mock HTTP server that serves `responses` in order, one per connection, and records
/// what each request carried. Extra connections are answered with the last response, so a
/// test that expects two requests fails loudly (on the assertion) rather than hanging if
/// a third arrives.
fn mock_server(responses: Vec<String>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);

    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());

            // Request line + headers, to the blank line.
            let mut authorization = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                // Header *names* are case-insensitive, but the value must keep its case —
                // lowercasing it would mangle the base64 of a Basic credential.
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                match name.to_ascii_lowercase().as_str() {
                    "authorization" => authorization = value.trim().to_owned(),
                    "content-length" => content_length = value.trim().parse().unwrap_or(0),
                    _ => {}
                }
            }

            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                let _ = reader.read_exact(&mut body);
            }
            recorder.lock().unwrap().push(Seen {
                authorization,
                body: String::from_utf8_lossy(&body).into_owned(),
            });

            let response = responses
                .get(index)
                .or_else(|| responses.last())
                .cloned()
                .unwrap_or_default();
            let _ = stream.write_all(response.as_bytes());
        }
    });

    (format!("http://{addr}"), seen)
}

/// An HTTP/1.1 response with `Connection: close`, so reqwest opens a fresh connection per
/// request and the server sees each one separately.
fn http(status_line: &str, headers: &[(&str, &str)], body: &str) -> String {
    let extra = headers
        .iter()
        .fold(String::new(), |mut out, (name, value)| {
            use std::fmt::Write as _;
            let _ = write!(out, "{name}: {value}\r\n");
            out
        });
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

/// Fastmail's real challenge, captured verbatim from `api.fastmail.com`.
const FASTMAIL_CHALLENGE: &str = r#"Bearer resource_metadata="https://api.fastmail.com/.well-known/oauth-protected-resource/jmap/session""#;

fn unauthorized(challenge: &str) -> String {
    http(
        "401 Unauthorized",
        &[("WWW-Authenticate", challenge)],
        "Invalid Authorization header, not bearer",
    )
}

fn ok(body: &str) -> String {
    http("200 OK", &[], body)
}

/// `Basic base64("alice@example.com:s3cret")`, the header a Basic framing produces.
const EXPECTED_BASIC: &str = "Basic YWxpY2VAZXhhbXBsZS5jb206czNjcmV0";
/// The same secret framed as a bearer token — the username is dropped, not encoded.
const EXPECTED_BEARER: &str = "Bearer s3cret";

fn transport() -> Transport {
    Transport::new(Credentials::basic("alice@example.com", "s3cret"), tls()).unwrap()
}

#[tokio::test]
async fn a_bearer_only_challenge_replays_the_request_as_bearer() {
    // The exact Fastmail failure: a password-shaped credential against a server that
    // takes only bearer tokens. Before challenge negotiation this surfaced to the user as
    // "JMAP HTTP 401: Invalid Authorization header, not bearer".
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        ok(r#"{"apiUrl":"/jmap/"}"#),
    ]);

    let response = transport().get(&base).await.unwrap();
    assert_eq!(response.status(), 200);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "the request should have been replayed once");
    assert_eq!(seen[0].authorization, EXPECTED_BASIC);
    assert_eq!(seen[1].authorization, EXPECTED_BEARER);
}

#[tokio::test]
async fn the_negotiated_scheme_is_latched_for_later_requests() {
    // Without latching every subsequent request would pay the same wasted round trip.
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        ok(r#"{"apiUrl":"/jmap/"}"#),
        ok(r#"{"ok":true}"#),
    ]);

    let transport = transport();
    transport.get(&base).await.unwrap();
    transport.get(&base).await.unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "only the first request should negotiate");
    assert_eq!(seen[2].authorization, EXPECTED_BEARER);
}

#[tokio::test]
async fn a_basic_challenge_is_not_retried_and_the_401_stands() {
    // Stalwart's shape. The scheme was right and the credential was wrong; retrying would
    // be a wasted round trip, and switching would bury the real cause.
    let (base, seen) = mock_server(vec![unauthorized(r#"Basic realm="jmap""#)]);

    let response = transport().get(&base).await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "no retry should be attempted"
    );
}

#[tokio::test]
async fn a_401_without_a_challenge_is_not_retried() {
    let (base, seen) = mock_server(vec![http("401 Unauthorized", &[], "nope")]);

    let response = transport().get(&base).await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_bearer_credential_is_not_downgraded_to_basic() {
    // A bare token has no username to build a Basic header from, so the 401 stands.
    let (base, seen) = mock_server(vec![unauthorized(r#"Basic realm="jmap""#)]);

    let transport = Transport::new(Credentials::bearer("tok"), tls()).unwrap();
    let response = transport.get(&base).await.unwrap();

    assert_eq!(response.status(), 401);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].authorization, "Bearer tok");
}

#[tokio::test]
async fn a_replayed_post_carries_the_same_body() {
    // The retry re-sends the request, so a method-call envelope must survive the switch
    // intact — a replay that dropped the body would fail in a far more confusing way.
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        ok(r#"{"methodResponses":[]}"#),
    ]);
    let envelope = json!({"using": ["urn:ietf:params:jmap:core"], "methodCalls": []});

    let value = transport().post_json(&base, &envelope).await.unwrap();
    assert!(value.get("methodResponses").is_some());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, envelope.to_string());
    assert_eq!(
        seen[1].body, seen[0].body,
        "the replay must carry the original body"
    );
    assert_eq!(seen[1].authorization, EXPECTED_BEARER);
}

#[tokio::test]
async fn a_replayed_blob_upload_carries_the_same_bytes() {
    // `post_bytes` sets a raw body rather than JSON; it must clone for replay too.
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        ok(r#"{"blobId":"G1"}"#),
    ]);

    let value = transport()
        .post_bytes(&base, "text/plain", b"attachment".to_vec())
        .await
        .unwrap();
    assert_eq!(value.get("blobId").unwrap(), "G1");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].body, "attachment");
}
