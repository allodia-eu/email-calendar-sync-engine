//! A server that stops answering, at each phase of an exchange, and one that is slow but
//! never stops.
//!
//! Driven on tokio's paused clock, which jumps to the next timer whenever every task is
//! waiting, so a ten-minute bound costs no wall time. Each run sits under [`GUARD`]: a phase
//! with no bound of its own reaches that timer instead, and fails as a hang rather than
//! hanging the suite.
//!
//! ⚠️ The paused clock also jumps while a task waits for a socket that already has bytes for
//! it: tokio advances to the next timer on every wait, whatever the socket held. So where a
//! server *answers*, the time it took is not measured exactly, and a test whose server must
//! keep a large transfer moving drives the body directly rather than through a socket.

use std::{sync::Arc, time::Duration};

use bytes::Bytes;
use engine_provider::Deadlines;
use http_body::Body as _;
use tokio::{net::TcpListener, time::Instant};

use super::{PIECE, Pieces, Progress, quiet};
use crate::{
    Exchange, RetryConfig, SendError, Sent, chunk_within, send_retrying,
    test_server::{SilentServer, reply, trickling},
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

const BOUNDS: Deadlines = Deadlines::STANDARD;

fn client() -> reqwest::Client {
    crate::client(&engine_tls::TlsClientConfig::bundled())
        .build()
        .expect("client")
}

/// Runs `operation` under [`GUARD`], returning its result and how long it took.
async fn timed<T>(operation: impl Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let outcome = tokio::time::timeout(GUARD, operation)
        .await
        .expect("hung: nothing bounded the wait on a silent server");
    (outcome, started.elapsed())
}

/// Asserts `elapsed` is `bound`, to the timer's millisecond resolution.
fn assert_took(elapsed: Duration, bound: Duration) {
    assert!(
        elapsed >= bound && elapsed < bound + Duration::from_secs(1),
        "expected {bound:?}, took {elapsed:?}"
    );
}

fn timed_out(outcome: Result<Sent, SendError>) -> SendError {
    let err = outcome.expect_err("a silent server is a failure");
    assert!(err.is_timeout(), "{err}");
    err
}

#[tokio::test(start_paused = true)]
async fn a_silent_server_fails_an_ordinary_request_after_the_reply_bound() {
    let server = SilentServer::start(Vec::new()).await;
    let request = client().get(server.url());

    let (outcome, elapsed) = timed(send_retrying(
        request,
        &RetryConfig::default(),
        Exchange::Ordinary,
    ))
    .await;

    let err = timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());
    assert_eq!(err.to_string(), "the server sent nothing for 60s");
    assert!(err.may_have_been_received(), "the request went out whole");
    assert_eq!(server.received(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_submission_waits_the_longer_bound_and_may_have_been_received() {
    let server = SilentServer::start(Vec::new()).await;
    let request = client().post(server.url()).body("a message");

    let (outcome, elapsed) = timed(send_retrying(
        request,
        &RetryConfig::default(),
        Exchange::Submission,
    ))
    .await;

    let err = timed_out(outcome);
    // Counted from the body's last piece, which goes out once the connection is up, and the
    // paused clock may already have jumped towards the dial bound by then.
    assert!(
        elapsed >= BOUNDS.submission() && elapsed < BOUNDS.submission() + BOUNDS.dial(),
        "{elapsed:?}"
    );
    assert!(err.may_have_been_received(), "{err}");
    assert_eq!(server.received(), 1, "sent once, never again");
}

#[tokio::test(start_paused = true)]
async fn a_server_that_stops_taking_the_body_never_received_it() {
    // Accepted and never read: the body fills the socket buffers, and its end never leaves.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let request = client().post(url).body(vec![b'x'; 32 * 1024 * 1024]);

    let (outcome, elapsed) = timed(send_retrying(
        request,
        &RetryConfig::default(),
        Exchange::Submission,
    ))
    .await;

    let err = timed_out(outcome);
    // At least the stall bound, from the last piece the buffers took.
    assert!(elapsed >= BOUNDS.stall(), "{elapsed:?}");
    assert!(err.to_string().contains("took nothing"), "{err}");
    assert!(!err.may_have_been_received(), "{err}");
    drop(listener);
}

#[tokio::test(start_paused = true)]
async fn an_upload_that_keeps_moving_is_never_cut_off() {
    // A piece taken every fifty seconds: never silent for a stall, and in all far longer than
    // any one bound. Once the last piece is taken, the reply bound runs from there.
    let progress = Arc::new(Progress::new(false));
    let mut body = Pieces {
        rest: Bytes::from(vec![b'x'; 20 * PIECE]),
        progress: Arc::clone(&progress),
    };
    let silence = quiet(&progress, Exchange::Submission.reply());
    tokio::pin!(silence);
    let started = Instant::now();

    while !body.is_end_stream() {
        tokio::select! {
            silent = &mut silence => panic!("cut off while moving: {silent}"),
            () = tokio::time::sleep(Duration::from_secs(50)) => {}
        }
        let piece = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await;
        assert!(piece.is_some());
    }
    let moving = started.elapsed();
    let (silent, waited) = timed(silence).await;

    assert!(moving > BOUNDS.submission(), "{moving:?}");
    assert_took(waited, BOUNDS.submission());
    assert!(silent.may_have_been_received(), "{silent}");
}

#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_moving_is_never_cut_off() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let url = trickling(head, "x".repeat(1000), 5, Duration::from_secs(50)).await;
    let request = client().get(url);

    let (outcome, elapsed) = timed(async {
        let sent = send_retrying(request, &RetryConfig::default(), Exchange::Ordinary).await?;
        sent.bytes().await
    })
    .await;

    assert_eq!(outcome.expect("a steady body arrives").len(), 5000);
    assert!(elapsed > BOUNDS.reply() + BOUNDS.stall(), "{elapsed:?}");
}

#[tokio::test(start_paused = true)]
async fn a_body_that_stops_fails_after_the_stall_bound() {
    let head = "HTTP/1.1 200 OK\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let url = trickling(head, "x".repeat(1000), 1, Duration::ZERO).await;
    let sent = send_retrying(
        client().get(url),
        &RetryConfig::default(),
        Exchange::Ordinary,
    )
    .await
    .expect("the head arrives");

    let (outcome, elapsed) = timed(sent.text()).await;

    let err = outcome.expect_err("a body that stops is a failure");
    assert!(err.is_timeout(), "{err}");
    assert!(err.may_have_been_received(), "a reply had begun");
    assert_took(elapsed, BOUNDS.stall());
}

#[tokio::test(start_paused = true)]
async fn a_body_read_to_classify_a_refusal_is_bounded_too() {
    const READS: &[u16] = &[403];
    let classifier = (READS, |_: u16, _: &[u8]| Some(crate::Throttle::new()));
    let retry = RetryConfig::default().classifying(Arc::new(classifier));
    let head = "HTTP/1.1 403 Forbidden\r\nContent-Length: 5000\r\n\r\n".to_owned();
    let url = trickling(head, "x".repeat(10), 1, Duration::ZERO).await;

    let (outcome, elapsed) =
        timed(send_retrying(client().get(url), &retry, Exchange::Ordinary)).await;

    let err = timed_out(outcome);
    assert!(elapsed >= BOUNDS.stall(), "{elapsed:?}");
    assert!(err.may_have_been_received(), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_stream_is_read_under_the_bound_its_caller_names() {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".to_owned();
    let url = trickling(head, "x".repeat(10), 1, Duration::from_mins(2)).await;
    let sent = send_retrying(
        client().get(url),
        &RetryConfig::default(),
        Exchange::Ordinary,
    )
    .await
    .expect("the head arrives");
    let mut stream = sent.into_streaming();
    let keepalive = Duration::from_mins(3);

    let first = timed(chunk_within(&mut stream, keepalive)).await;
    let (outcome, elapsed) = timed(chunk_within(&mut stream, keepalive)).await;

    assert!(first.0.expect("a ping after two minutes").is_some());
    assert!(
        first.1 > BOUNDS.stall(),
        "the stall bound is not the stream's"
    );
    assert!(outcome.expect_err("then nothing").is_timeout());
    assert_took(elapsed, keepalive);
}

#[tokio::test(start_paused = true)]
async fn a_connection_whose_handshake_never_completes_fails_after_the_dial_bound() {
    // Bound and never accepted: the kernel completes the TCP connect, and nothing answers
    // the TLS hello.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("https://{}/", listener.local_addr().expect("address"));
    let request = client().post(url).body("a message");

    let (outcome, elapsed) = timed(send_retrying(
        request,
        &RetryConfig::default(),
        Exchange::Submission,
    ))
    .await;

    let err = timed_out(outcome);
    assert_took(elapsed, BOUNDS.dial());
    assert!(!err.may_have_been_received(), "{err}");
    drop(listener);
}

#[tokio::test]
async fn text_is_decoded_by_the_charset_the_reply_names() {
    // The body is `café` in UTF-8, five bytes, under a header that says Latin-1. Decoded as
    // the header says, the two bytes of the `é` are two characters.
    let server = SilentServer::start(vec![reply(
        "200 OK",
        "text/plain; charset=iso-8859-1",
        "caf\u{e9}",
    )])
    .await;
    let sent = send_retrying(
        client().get(server.url()),
        &RetryConfig::default(),
        Exchange::Ordinary,
    )
    .await
    .expect("sent");

    let text = sent.text().await.expect("text");

    assert_eq!(text, "caf\u{c3}\u{a9}");
}
