//! JMAP against a server that stops answering: a method call, the request that submits a
//! message, and a blob that arrives slowly but steadily.
//!
//! Through the real client. The session is fetched in real time and the clock is paused once
//! it is, because on the paused clock a server that answers spends time it never spent
//! (`engine_http::test_server`). Each wait sits under a guard that turns a missing bound into
//! a failure rather than a hang.

use std::time::Duration;

use async_trait::async_trait;
use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_http::{
    Exchange,
    test_server::{SilentServer, reply, trickling},
};
use engine_provider::{Deadlines, Draft, ProviderError};
use tokio::time::Instant;

use crate::{
    Credentials, JmapClient, JmapConfig,
    error::JmapError,
    executor::Executor,
    provider::provider_test_support::{FakeExecutor, fixture},
    request::{Request, Response, capability},
    session::Session,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

const BOUNDS: Deadlines = Deadlines::STANDARD;

const SESSION: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{},"urn:ietf:params:jmap:submission":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c","urn:ietf:params:jmap:submission":"c"},"apiUrl":"https://mail.test.local/jmap/"}"#;

/// Runs `operation` under [`GUARD`], returning its result and how long it took.
async fn timed<T>(operation: impl Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let outcome = tokio::time::timeout(GUARD, operation)
        .await
        .expect("hung: nothing bounded the wait on a silent server");
    (outcome, started.elapsed())
}

/// A client whose session `server` served, connected in real time.
async fn connected(server: &SilentServer) -> JmapClient {
    let config = JmapConfig::new(server.url(), Credentials::basic("alice", "pw"))
        .with_session_path("/jmap/session");
    JmapClient::connect(config).await.expect("session")
}

/// A server that serves the session and then nothing.
async fn session_then_silence() -> SilentServer {
    SilentServer::start(vec![reply("200 OK", "application/json", SESSION)]).await
}

/// The submission's own request goes to the live client; the read before it is canned,
/// so the silence is the submission's alone.
struct SilentSubmission {
    canned: FakeExecutor,
    live: JmapClient,
}

#[async_trait]
impl Executor for SilentSubmission {
    async fn execute(&self, request: &Request) -> Result<Response, JmapError> {
        self.canned.execute(request).await
    }

    async fn submit(&self, request: &Request) -> Result<Response, JmapError> {
        Executor::submit(&self.live, request).await
    }

    async fn download(&self, _url: &str) -> Result<Vec<u8>, JmapError> {
        unreachable!("a send without attachments downloads nothing")
    }

    async fn upload(&self, _url: &str, _type: &str, _bytes: &[u8]) -> Result<String, JmapError> {
        unreachable!("a send without attachments uploads nothing")
    }

    fn session(&self) -> &Session {
        self.live.session()
    }
}

#[tokio::test]
async fn a_silent_server_fails_a_method_call_retryable_after_the_reply_bound() {
    let server = session_then_silence().await;
    let client = connected(&server).await;
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    request.invoke("Mailbox/get", serde_json::json!({ "accountId": "c" }));
    tokio::time::pause();

    let (outcome, elapsed) = timed(client.execute(&request, Exchange::Ordinary)).await;

    let err = ProviderError::from(outcome.expect_err("a silent server is a failure"));
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert!(elapsed >= BOUNDS.reply() && elapsed < BOUNDS.reply() + Duration::from_secs(1));
}

#[tokio::test]
async fn a_submission_the_server_never_answers_needs_confirmation_and_is_never_retried() {
    let server = session_then_silence().await;
    let executor = SilentSubmission {
        canned: FakeExecutor::new(vec![fixture("submit_context_response.json")]),
        live: connected(&server).await,
    };
    let draft = Draft::new(
        MessageIdHeader::new("jmap-silent-0001@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    );
    tokio::time::pause();

    let (outcome, elapsed) = timed(crate::submit::send(&executor, "c", "c", &draft)).await;

    let err = outcome.expect_err("an unanswered send is not a success");
    assert!(err.requires_confirmation(), "{err}");
    assert!(!err.is_retryable(), "{err}");
    assert_eq!(
        server.received(),
        2,
        "the session, then the send once and never again"
    );
    assert!(elapsed >= BOUNDS.submission(), "{elapsed:?}");
    assert!(elapsed < BOUNDS.submission() + BOUNDS.dial(), "{elapsed:?}");
}

#[tokio::test]
async fn a_blob_that_keeps_arriving_is_never_cut_off() {
    let server = session_then_silence().await;
    let client = connected(&server).await;
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let blobs = trickling(head, "x".repeat(1000), 5, Duration::from_secs(50)).await;
    tokio::time::pause();

    let (outcome, elapsed) = timed(client.download(&format!("{blobs}/download/c/b/m"))).await;

    assert_eq!(outcome.expect("a steady blob arrives").len(), 5000);
    assert!(elapsed > BOUNDS.reply() + BOUNDS.stall(), "{elapsed:?}");
}
