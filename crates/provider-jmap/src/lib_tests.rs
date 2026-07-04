//! Unit tests for the JMAP client handle ([`super::JmapClient`]) — credential
//! redaction, session discovery over a mock HTTP server, and the parse pipeline's
//! hostile-input guards. Split out to keep `lib.rs` under the 500-line limit.

use super::*;

#[test]
fn credentials_debug_is_redacted() {
    let basic = Credentials::basic("alice@test.local", "harness-alice-pw");
    let shown = format!("{basic:?}");
    assert!(shown.contains("alice@test.local"));
    assert!(
        !shown.contains("harness-alice-pw"),
        "password must not leak: {shown}"
    );
    let bearer = Credentials::bearer("super-secret-token");
    let shown = format!("{bearer:?}");
    assert!(
        !shown.contains("super-secret-token"),
        "token must not leak: {shown}"
    );
}

#[test]
fn config_debug_omits_credentials() {
    let config = JmapConfig::new(
        "http://127.0.0.1:18080",
        Credentials::basic("alice@test.local", "harness-alice-pw"),
    );
    let shown = format!("{config:?}");
    assert!(shown.contains("127.0.0.1:18080"));
    assert!(!shown.contains("harness-alice-pw"));
}

#[test]
fn config_builder_overrides_defaults() {
    let config = JmapConfig::new("http://h", Credentials::bearer("t"))
        .with_session_path("/jmap/session")
        .with_session_urls(SessionUrlPolicy::TrustAdvertised);
    assert_eq!(config.session_path, "/jmap/session");
    assert_eq!(config.session_urls, SessionUrlPolicy::TrustAdvertised);
}

/// Hostile-input guard (the `fuzz/` cargo-fuzz target's in-gate counterpart):
/// the JMAP parsers must return errors, never panic, on adversarial JSON.
#[test]
fn parsers_never_panic_on_hostile_json() {
    use serde_json::json;
    let adversarial = [
        json!(null),
        json!(7),
        json!("x"),
        json!([]),
        json!({}),
        json!({ "id": 123 }),
        json!({ "mailboxIds": "nope", "keywords": 5 }),
        json!({ "id": "e", "calendarIds": { "c": true }, "start": "not-a-date" }),
        json!({ "id": "e", "uid": "u", "calendarIds": { "c": true }, "start": "2026-13-40T99:99:99" }),
        json!({ "recurrenceRule": { "frequency": "fortnightly" } }),
        json!({ "id": "e", "uid": "u", "calendarIds": { "c": true }, "start": "2026-01-01T00:00:00",
                    "recurrenceOverrides": { "bad-rid": { "start": "also-bad" } } }),
        json!({ "methodResponses": "not-an-array" }),
        json!({ "methodResponses": [["only-two", {}]] }),
        json!({ "created": [1, 2, 3], "newState": 9 }),
        json!({ "participants": { "p": { "calendarAddress": 5, "roles": "nope" } } }),
    ];
    for case in &adversarial {
        let _ = mail::mailbox_from_json(case);
        let _ = mail::message_from_json(case);
        let _ = calendar::calendar_from_json(case);
        let _ = calendar::event_from_json(case);
        let _ = request::Response::parse(case);
        let _ = sync_ops::Changes::parse(case);
    }
}

#[test]
fn raw_bytes_never_panic_through_the_pipeline() {
    for raw in [
        b"".as_slice(),
        b"{",
        b"[1,2,",
        b"\xff\xfe\x00",
        b"1e9999",
        br#"{"start":"2026-02-30T00:00:00","timeZone":""}"#,
    ] {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) {
            let _ = calendar::event_from_json(&value);
            let _ = mail::message_from_json(&value);
        }
    }
}

#[cfg(feature = "fuzzing")]
#[test]
fn fuzz_entry_point_runs_without_panicking() {
    // Drive the fuzz entry the cargo-fuzz target calls, so it is covered under
    // `--all-features` even without nightly.
    for raw in [
        br#"{"id":"e","mailboxIds":{"a":true}}"#.as_slice(),
        b"garbage",
        b"{}",
    ] {
        fuzz_parse(raw);
    }
}

// A blocking single-shot mock HTTP server lets the live-only transport,
// session discovery, and `execute` be exercised offline (no harness).
fn mock_server(http_responses: Vec<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for response in http_responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A `200 application/octet-stream` response — the raw-blob download shape.
fn http_bytes(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

const SESSION_DOC: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{"maxObjectsInGet":500},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"https://mail.test.local/jmap/"}"#;

#[tokio::test]
async fn connect_and_execute_against_a_mock_server() {
    let api = r#"{"methodResponses":[["Mailbox/get",{"state":"s1","list":[]},"0"]]}"#;
    let base = mock_server(vec![http_ok(SESSION_DOC), http_ok(api)]);
    let client = JmapClient::connect(
        JmapConfig::new(base, Credentials::basic("alice", "pw")).with_session_path("/jmap/session"),
    )
    .await
    .unwrap();
    assert!(client.session().capabilities().mail());
    assert!(format!("{client:?}").contains("JmapClient"));

    let mut req = request::Request::new([request::capability::CORE]);
    req.invoke("Mailbox/get", serde_json::json!({ "accountId": "c" }));
    let resp = client.execute(&req).await.unwrap();
    assert!(resp.result("0").is_ok());
}

#[tokio::test]
async fn connect_follows_the_well_known_redirect() {
    let redirect = "HTTP/1.1 307 Temporary Redirect\r\nLocation: /jmap/session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let base = mock_server(vec![redirect.to_owned(), http_ok(SESSION_DOC)]);
    // Default session path is /.well-known/jmap → 307 → /jmap/session (rebased).
    let client = JmapClient::connect(JmapConfig::new(base, Credentials::basic("a", "b")))
        .await
        .unwrap();
    assert!(client.session().capabilities().mail());
}

#[tokio::test]
async fn http_error_status_surfaces_as_a_classified_error() {
    let body =
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 3\r\nConnection: close\r\n\r\nerr";
    let base = mock_server(vec![body.to_owned()]);
    let err = JmapClient::connect(
        JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Retryable
    );
}

#[tokio::test]
async fn jmap_provider_connects_and_syncs_through_the_real_client() {
    use engine_provider::Provider;
    let mailboxes = r#"{"methodResponses":[["Mailbox/get",{"state":"s1","list":[]},"0"]]}"#;
    let base = mock_server(vec![http_ok(SESSION_DOC), http_ok(mailboxes)]);
    let provider = JmapProvider::connect(
        JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
    )
    .await
    .unwrap();
    assert!(format!("{provider:?}").contains("JmapProvider"));
    let account = engine_core::ids::AccountId::try_from("acct").unwrap();
    assert!(
        provider
            .sync_mailboxes(&account, None)
            .await
            .unwrap()
            .is_snapshot()
    );
}

#[tokio::test]
async fn transport_connect_failure_is_retryable() {
    // A refused connection surfaces as a retryable transport error.
    let err = JmapClient::connect(
        JmapConfig::new("http://127.0.0.1:1", Credentials::basic("a", "b"))
            .with_session_path("/jmap/session"),
    )
    .await
    .unwrap_err();
    assert!(err.failure_class().is_retryable());
}

#[tokio::test]
async fn malformed_session_body_is_a_permanent_decode_error() {
    let base = mock_server(vec![http_ok("this is not json")]);
    let err = JmapClient::connect(
        JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session"),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}

#[tokio::test]
async fn bearer_auth_connects() {
    let base = mock_server(vec![http_ok(SESSION_DOC)]);
    let client = JmapClient::connect(
        JmapConfig::new(base, Credentials::bearer("tok")).with_session_path("/jmap/session"),
    )
    .await
    .unwrap();
    assert!(client.session().capabilities().mail());
}

// --- Blob download / upload over the real reqwest transport (mock server, no Docker).
// Exercises `Transport::get_bytes`/`post_bytes` + `JmapClient::download`/`upload` + the
// `Executor` delegation end-to-end, the paths otherwise only hit by the live tests.

/// A session advertising the download + upload + submission surface these tests need.
const RICH_SESSION_DOC: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{"maxObjectsInGet":500},"urn:ietf:params:jmap:mail":{},"urn:ietf:params:jmap:submission":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c","urn:ietf:params:jmap:submission":"c"},"apiUrl":"https://mail.test.local/jmap/","downloadUrl":"https://mail.test.local/download/{accountId}/{blobId}/{name}?accept={type}","uploadUrl":"https://mail.test.local/upload/{accountId}/"}"#;

fn rich_config(base: String) -> JmapConfig {
    JmapConfig::new(base, Credentials::basic("a", "b")).with_session_path("/jmap/session")
}

#[tokio::test]
async fn fetch_message_source_downloads_the_blob_through_the_real_client() {
    use engine_core::ids::{AccountId, BlobId, MailboxId, MessageId};
    use engine_core::mail::Message;
    use engine_core::membership::Memberships;
    use engine_provider::Provider;

    let raw = "From: a@test.local\r\nSubject: probe\r\n\r\nbody";
    let base = mock_server(vec![http_ok(RICH_SESSION_DOC), http_bytes(raw)]);
    let provider = JmapProvider::connect(rich_config(base)).await.unwrap();

    let mut msg = Message::new(
        MessageId::try_from("e1").unwrap(),
        Memberships::of_one(MailboxId::try_from("mb").unwrap()),
    );
    msg.blob_id = Some(BlobId::try_from("blob-1").unwrap());
    let raw_mime = provider
        .fetch_message_source(&AccountId::try_from("acct").unwrap(), &msg)
        .await
        .unwrap();
    // The GET body came back verbatim through get_bytes → download → the Executor.
    assert_eq!(raw_mime.as_bytes(), raw.as_bytes());
}

#[tokio::test]
async fn submit_email_uploads_the_attachment_blob_through_the_real_client() {
    use engine_core::ids::MessageIdHeader;
    use engine_core::mail::EmailAddress;
    use engine_provider::{Draft, DraftAttachment, Provider};

    // connect(session) → resolve_context(Mailbox/Identity) → upload(blob) → send.
    let context = r#"{"methodResponses":[["Mailbox/get",{"list":[{"id":"d","name":"Drafts","role":"drafts"},{"id":"s","name":"Sent","role":"sent"}]},"0"],["Identity/get",{"list":[{"id":"id1"}]},"1"]]}"#;
    let sent = r#"{"methodResponses":[["Email/set",{"created":{"draft":{"id":"e9"}}},"0"],["EmailSubmission/set",{"created":{"sub":{"id":"sub1"}}},"1"]]}"#;
    let base = mock_server(vec![
        http_ok(RICH_SESSION_DOC),
        http_ok(context),
        http_ok(r#"{"blobId":"blob-att","type":"application/pdf","size":3}"#),
        http_ok(sent),
    ]);
    let provider = JmapProvider::connect(rich_config(base)).await.unwrap();

    let draft = Draft::new(
        MessageIdHeader::new("m@test.local").unwrap(),
        EmailAddress::new("a@test.local"),
        vec![EmailAddress::new("b@test.local")],
        "subject",
        "body",
    )
    .with_attachment(DraftAttachment::attachment(
        "r.pdf",
        "application/pdf",
        vec![1, 2, 3],
    ));
    let receipt = provider
        .submit_email(
            &engine_core::ids::AccountId::try_from("acct").unwrap(),
            &draft,
        )
        .await
        .unwrap();
    assert_eq!(receipt.email_key.as_str(), "e9");
}

#[tokio::test]
async fn upload_without_a_blob_id_is_a_protocol_error() {
    // The upload endpoint returns a body missing `blobId` — a permanent protocol error.
    let base = mock_server(vec![http_ok(RICH_SESSION_DOC), http_ok("{}")]);
    let client = JmapClient::connect(rich_config(base.clone()))
        .await
        .unwrap();
    let err = client
        .upload(&format!("{base}/upload/c/"), "text/plain", b"x")
        .await
        .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::Permanent
    );
}
