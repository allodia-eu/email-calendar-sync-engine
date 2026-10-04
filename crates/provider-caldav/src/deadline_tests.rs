//! A DAV server that stops answering: a report, a write, and a body that arrives slowly but
//! steadily. DAV submits no message, so there is no ambiguous send here: a write that loses
//! its reply is retried, as one that loses its connection always was, and the precondition
//! it carries is what keeps a replay from applying it twice.
//!
//! Through the real transport, on tokio's paused clock, under a guard that turns a missing
//! bound into a failure rather than a hang (`engine_http::test_server` has the trap the
//! paused clock sets for a server that answers).

use std::time::Duration;

use engine_http::test_server::{SilentServer, trickling};
use engine_provider::{Deadlines, ProviderError};
use tokio::time::Instant;

use super::{DavClient, DavExecutor, DavMethod, Precondition, WriteRequest};
use crate::Credentials;

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

fn client(base: &str) -> DavClient {
    DavClient::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "pw".to_owned(),
        },
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

/// Asserts a silent server failed `err` retryable, after the reply bound counted from the
/// request body's last piece, which goes out once the connection is up: by then the paused
/// clock may already have jumped towards the dial bound.
fn assert_retryable_after_the_reply_bound(err: &ProviderError, elapsed: Duration) {
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert!(elapsed >= BOUNDS.reply(), "{elapsed:?}");
    assert!(elapsed < BOUNDS.reply() + BOUNDS.dial(), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn a_silent_server_fails_a_report_retryable_after_the_reply_bound() {
    let server = SilentServer::start(Vec::new()).await;
    let client = client(server.url());
    let body = "<sync-collection xmlns=\"DAV:\"/>".to_owned();

    let (outcome, elapsed) =
        timed(client.send(DavMethod::Report, "/calendars/alice/", "1", body)).await;

    let err = ProviderError::from(outcome.expect_err("a silent server is a failure"));
    assert_retryable_after_the_reply_bound(&err, elapsed);
}

#[tokio::test(start_paused = true)]
async fn a_write_whose_reply_never_comes_is_retried_under_its_precondition() {
    let server = SilentServer::start(Vec::new()).await;
    let client = client(server.url());
    let write = WriteRequest {
        method: DavMethod::Put,
        href: "/calendars/alice/default/e.ics".to_owned(),
        content_type: Some("text/calendar; charset=utf-8"),
        precondition: Precondition::IfNoneMatch,
        body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
    };

    let (outcome, elapsed) = timed(client.send_write(write)).await;

    let err = ProviderError::from(outcome.expect_err("a silent server is a failure"));
    assert_retryable_after_the_reply_bound(&err, elapsed);
}

#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_arriving_is_never_cut_off() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let base = trickling(head, "x".repeat(1000), 5, Duration::from_secs(50)).await;
    let client = client(&base);

    let (outcome, elapsed) = timed(client.get_bytes(&format!("{base}/photo.png"))).await;

    assert_eq!(outcome.expect("a steady body arrives").len(), 5000);
    assert!(elapsed > BOUNDS.reply() + BOUNDS.stall(), "{elapsed:?}");
}
