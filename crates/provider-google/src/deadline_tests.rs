//! Google against a server that stops answering: a read, a `messages.send` it never answers,
//! one whose connection never completes, and a body that arrives slowly but steadily.
//!
//! Through the real transport, on tokio's paused clock, under a guard that turns a missing
//! bound into a failure rather than a hang (`engine_http::test_server` has the trap the
//! paused clock sets for a server that answers).

use std::time::Duration;

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_http::test_server::{SilentServer, held_until, reply, trickling};
use engine_provider::{Deadlines, Draft, ProviderError};
use tokio::{net::TcpListener, time::Instant};

use crate::{
    GoogleClient,
    named_labels::Labels,
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

fn client(base: &str) -> GoogleClient {
    GoogleClient::with_base("token", base, tls(), &retry()).expect("client")
}

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("google-silent-0001@test.local").unwrap(),
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

    let (outcome, elapsed) = held_until(
        server.has_received(1),
        timed(client.get(&client.url("/gmail/v1/users/me/labels"))),
    )
    .await;

    let err = ProviderError::from(outcome.expect_err("a silent server is a failure"));
    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    assert!(elapsed >= BOUNDS.reply() && elapsed < BOUNDS.reply() + Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn a_send_the_server_never_answers_needs_confirmation_and_is_never_retried() {
    let server = SilentServer::start(Vec::new()).await;
    let client = client(server.url());

    let hand_over = engine_provider::HandOver::new(&engine_provider::Unrecorded);
    let (outcome, elapsed) = held_until(
        server.has_received(1),
        timed(crate::submit::send(
            &client,
            &draft(),
            &Labels::default(),
            &hand_over,
        )),
    )
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
        &Labels::default(),
        &engine_provider::HandOver::new(&engine_provider::Unrecorded),
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

    let (outcome, elapsed) = timed(client.get_bytes(&format!("{base}/photo"))).await;

    assert_eq!(outcome.expect("a steady body arrives").len(), 5000);
    assert!(elapsed > BOUNDS.reply() + BOUNDS.stall(), "{elapsed:?}");
}

/// A hand-over log that notes how many requests the server had read in full when the
/// hand-over was recorded, after giving anything already sent time to land.
struct Watch<'a> {
    server: &'a SilentServer,
    seen: std::sync::Mutex<Option<usize>>,
    refuse: bool,
}

impl<'a> Watch<'a> {
    fn on(server: &'a SilentServer, refuse: bool) -> Self {
        Self {
            server,
            seen: std::sync::Mutex::new(None),
            refuse,
        }
    }

    fn seen(&self) -> Option<usize> {
        *self.seen.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl engine_provider::HandOverLog for Watch<'_> {
    async fn record_hand_over(&self) -> Result<(), String> {
        tokio::time::sleep(Duration::from_millis(100)).await;
        *self.seen.lock().unwrap() = Some(self.server.received());
        if self.refuse {
            return Err("disk full".to_owned());
        }
        Ok(())
    }
}

/// `messages.send` is the one request that can deliver, and the hand-over is recorded inside
/// it: after the connection is up, before the server has the whole request.
#[tokio::test]
async fn the_hand_over_is_recorded_inside_the_send_request() {
    let server =
        SilentServer::start(vec![reply("200 OK", "application/json", r#"{"id":"g1"}"#)]).await;
    let client = client(server.url());
    let log = Watch::on(&server, false);
    let hand_over = engine_provider::HandOver::new(&log);

    let receipt = crate::submit::send(&client, &draft(), &Labels::default(), &hand_over)
        .await
        .expect("sent");

    assert_eq!(receipt.email_key.as_str(), "g1");
    assert_eq!(
        log.seen(),
        Some(0),
        "the request was not whole at the record"
    );
    assert_eq!(server.received(), 1);
}

#[tokio::test]
async fn a_hand_over_that_cannot_be_recorded_never_completes_the_send_request() {
    let server = SilentServer::start(vec![reply("200 OK", "application/json", "{}")]).await;
    let client = client(server.url());
    let log = Watch::on(&server, true);

    let err = crate::submit::send(
        &client,
        &draft(),
        &Labels::default(),
        &engine_provider::HandOver::new(&log),
    )
    .await
    .expect_err("withheld");

    assert!(err.is_retryable(), "{err}");
    assert!(!err.requires_confirmation(), "{err}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server.received(),
        0,
        "the server never had the whole request"
    );
}

/// After the hand-over, a gateway saying the server behind it gave no answer is the lost
/// answer it describes, and so is a success nobody can read; the server's own refusal is
/// definitive.
#[tokio::test]
async fn a_gateway_with_no_answer_asks_and_the_servers_own_refusal_does_not() {
    for (status, body, ambiguous) in [
        ("502 Bad Gateway", "{}", true),
        ("504 Gateway Timeout", "{}", true),
        ("200 OK", "not json", true),
        ("500 Internal Server Error", "{}", false),
        ("503 Service Unavailable", "{}", false),
    ] {
        let server = SilentServer::start(vec![reply(status, "application/json", body)]).await;
        let client = client(server.url());
        let err = crate::submit::send(
            &client,
            &draft(),
            &Labels::default(),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect_err(status);
        assert_eq!(err.requires_confirmation(), ambiguous, "{status}: {err}");
    }
}
