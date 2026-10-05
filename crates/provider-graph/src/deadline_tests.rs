//! Graph against a server that stops answering: a read, a `sendMail` it never answers, one
//! whose connection never completes, and a body that arrives slowly but steadily.
//!
//! Through the real transport, on tokio's paused clock, under a guard that turns a missing
//! bound into a failure rather than a hang (`engine_http::test_server` has the trap the
//! paused clock sets for a server that answers).

use std::time::Duration;

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_http::test_server::{SilentServer, trickling};
use engine_provider::{Deadlines, Draft, ProviderError};
use tokio::{net::TcpListener, time::Instant};

use crate::{
    GraphClient,
    categories::Categories,
    test_support::{retry, tls},
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

const BOUNDS: Deadlines = Deadlines::STANDARD;

/// Runs `operation` under [`GUARD`], returning its result and how long it took.
async fn timed<T>(operation: impl Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let outcome = tokio::time::timeout(GUARD, operation)
        .await
        .expect("hung: nothing bounded the wait on a silent server");
    (outcome, started.elapsed())
}

fn client(base: &str) -> GraphClient {
    GraphClient::with_base("token", base, tls(), &retry()).expect("client")
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("graph-silent-0001@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
}

#[tokio::test(start_paused = true)]
async fn a_silent_server_fails_a_read_retryable_after_the_reply_bound() {
    let server = SilentServer::start(Vec::new()).await;
    let client = client(server.url());

    let (outcome, elapsed) = timed(client.get(&client.url("/mailFolders"))).await;

    let err = ProviderError::from(outcome.expect_err("a silent server is a failure"));
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert!(elapsed >= BOUNDS.reply() && elapsed < BOUNDS.reply() + Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn a_send_the_server_never_answers_needs_confirmation_and_is_never_retried() {
    let server = SilentServer::start(Vec::new()).await;
    let client = client(server.url());

    let (outcome, elapsed) = timed(crate::submit::send(
        &client,
        &draft(),
        &Categories::default(),
    ))
    .await;

    let err = outcome.expect_err("an unanswered send is not a success");
    assert!(err.requires_confirmation(), "{err}");
    assert!(!err.is_retryable(), "{err}");
    assert_eq!(server.received(), 1, "sent once, never again");
    assert!(elapsed >= BOUNDS.submission(), "{elapsed:?}");
    assert!(elapsed < BOUNDS.submission() + BOUNDS.dial(), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn a_send_whose_connection_never_completes_is_retried() {
    // Bound and never accepted: the connect completes in the kernel and nothing answers the
    // TLS hello, so nothing can have been sent.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("https://{}", listener.local_addr().expect("address"));
    let client = client(&base);

    let (outcome, elapsed) = timed(crate::submit::send(
        &client,
        &draft(),
        &Categories::default(),
    ))
    .await;

    let err = outcome.expect_err("an unconnected send is not a success");
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert!(elapsed >= BOUNDS.dial() && elapsed < BOUNDS.dial() + Duration::from_secs(1));
    drop(listener);
}

#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_arriving_is_never_cut_off() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let base = trickling(head, "x".repeat(1000), 5, Duration::from_secs(50)).await;
    let client = client(&base);

    let (outcome, elapsed) = timed(client.get_bytes(&format!("{base}/me/messages/m/$value"))).await;

    assert_eq!(outcome.expect("a steady body arrives").len(), 5000);
    assert!(elapsed > BOUNDS.reply() + BOUNDS.stall(), "{elapsed:?}");
}
