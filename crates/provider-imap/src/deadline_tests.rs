//! A server that accepts the connection and then never answers, at each point a dial or a
//! submission waits on it.
//!
//! Driven on tokio's paused clock, which jumps to the next timer whenever every task is
//! waiting, so a one-minute bound costs no wall time. Each run sits under [`GUARD`]: a path
//! with no bound of its own reaches that timer instead, and fails as a hang rather than
//! hanging the suite.

use std::time::Duration;

use engine_core::error::FailureClass;
use tokio::{
    io::{AsyncWriteExt, duplex},
    net::{TcpListener, TcpStream},
    time::Instant,
};
use tokio_rustls::rustls::pki_types::ServerName;

use super::{DATA_ACK_STALL, DIAL_STALL, REPLY_STALL, handshake};
use crate::{
    credentials::Credentials,
    error::{ImapError, ImapResult},
    mock::{MockStream, script, written},
    smtp::{self, Disposition, SmtpResult},
    smtp_auth::SmtpAuth,
    transport::Connection,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

const GREETING: &str = "220 mail ESMTP\r\n";
const EHLO: &str = "250-mail\r\n250 AUTH PLAIN\r\n";
const MAIL: &str = "250 2.1.0 OK\r\n";
const RCPT: &str = "250 2.1.5 OK\r\n";
const GO_AHEAD: &str = "354 go ahead\r\n";

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

/// Asserts `outcome` failed as a silent server: timed out, and retryable.
fn assert_timed_out<T: std::fmt::Debug>(outcome: ImapResult<T>) -> ImapError {
    let err = outcome.expect_err("a silent server is a failure");
    assert!(
        matches!(&err, ImapError::Io(io) if io.kind() == std::io::ErrorKind::TimedOut),
        "{err:?}"
    );
    assert_eq!(err.failure_class(), FailureClass::Retryable);
    err
}

/// A plaintext submission of a one-line message over `stream`.
async fn submit(stream: MockStream, auth: Option<SmtpAuth<'_>>) -> ImapResult<SmtpResult> {
    let to = vec!["bob@test.local".to_owned()];
    smtp::send(
        stream,
        "test.local",
        "alice@test.local",
        &to,
        b"hi\r\n",
        auth,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_before_its_greeting_fails_retryable_after_the_reply_stall() {
    let (stream, _) = MockStream::silent_after(script(&[]));

    let (outcome, elapsed) = timed(submit(stream, None)).await;

    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_before_the_message_is_handed_over_fails_retryable() {
    // Each script ends where the server stops answering: before `EHLO`'s reply, `MAIL`'s,
    // `RCPT`'s, and the `354` that lets the message go. Nothing has been submitted at any of
    // them, so the outbox may send it again.
    let cases: [&[&str]; 4] = [
        &[GREETING],
        &[GREETING, EHLO],
        &[GREETING, EHLO, MAIL],
        &[GREETING, EHLO, MAIL, RCPT],
    ];
    for replies in cases {
        let (stream, recorded) = MockStream::silent_after(script(replies));

        let (outcome, elapsed) = timed(submit(stream, None)).await;

        assert_timed_out(outcome);
        assert_took(elapsed, REPLY_STALL);
        assert!(!written(&recorded).contains("hi\r\n"), "{replies:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_after_auth_fails_retryable() {
    let (stream, recorded) = MockStream::silent_after(script(&[GREETING, EHLO]));
    let credentials = Credentials::password("alice", "pw");
    let auth = SmtpAuth {
        credentials: &credentials,
        host: "mail.test.local",
        port: Some(465),
    };

    let (outcome, elapsed) = timed(submit(stream, Some(auth))).await;

    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);
    assert!(written(&recorded).contains("AUTH PLAIN"));
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_after_the_end_of_data_leaves_the_send_ambiguous() {
    let (stream, recorded) =
        MockStream::silent_after(script(&[GREETING, EHLO, MAIL, RCPT, GO_AHEAD]));

    let (outcome, elapsed) = timed(submit(stream, None)).await;

    // The message is on the wire: it may have been accepted, so this is never a failure the
    // outbox could retry, and it waits the longer bound before saying so.
    let result = outcome.expect("an unanswered end of data is a disposition, not an error");
    assert!(
        matches!(result.disposition, Disposition::Ambiguous(_)),
        "{:?}",
        result.disposition
    );
    assert_took(elapsed, DATA_ACK_STALL);
    assert!(written(&recorded).contains("hi\r\n.\r\n"));
}

#[tokio::test(start_paused = true)]
async fn a_server_that_stops_reading_the_message_fails_retryable() {
    // The server answers up to the `354` and then reads nothing, so the message fills the
    // pipe and the write stalls. Its end never reached the server, so nothing was submitted.
    let (client, mut server) = duplex(1024);
    for reply in [GREETING, EHLO, MAIL, RCPT, GO_AHEAD] {
        server.write_all(reply.as_bytes()).await.expect("reply");
    }
    let to = vec!["bob@test.local".to_owned()];
    let message = "a line of the message\r\n".repeat(10_000);

    let (outcome, elapsed) = timed(smtp::send(
        client,
        "test.local",
        "alice@test.local",
        &to,
        message.as_bytes(),
        None,
    ))
    .await;

    let err = assert_timed_out(outcome);
    assert!(err.to_string().contains("took nothing"), "{err}");
    assert_took(elapsed, REPLY_STALL);
    drop(server);
}

#[tokio::test(start_paused = true)]
async fn a_silent_submission_server_bounds_every_probe() {
    // Before the greeting.
    let (stream, _) = MockStream::silent_after(script(&[]));
    let (outcome, elapsed) = timed(smtp::extensions(stream, "test.local")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);

    // Before the `220` that lets the upgrade start.
    let ehlo = "250-mail\r\n250 STARTTLS\r\n";
    let (stream, _) = MockStream::silent_after(script(&[GREETING, ehlo]));
    let (outcome, elapsed) = timed(smtp::negotiate_starttls(stream, "test.local")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);

    // Over the upgraded link, before `EHLO`'s reply.
    let (stream, _) = MockStream::silent_after(script(&[]));
    let (outcome, elapsed) = timed(smtp::extensions_after_starttls(stream, "test.local")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);
}

#[tokio::test(start_paused = true)]
async fn an_imap_server_silent_before_its_greeting_fails_retryable() {
    let (stream, _) = MockStream::silent_after(script(&[]));

    let (outcome, elapsed) = timed(Connection::open(stream)).await;

    assert_timed_out(outcome);
    assert_took(elapsed, REPLY_STALL);
}

#[tokio::test]
async fn a_server_silent_through_the_tls_handshake_fails_retryable_after_the_dial_stall() {
    // A real socket, because the handshake is rustls driving one. The clock is paused only once
    // the connection is up, so the bound under test is the handshake's alone.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let (_accepted, _) = listener.accept().await.expect("accept");
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    let name = ServerName::try_from("mail.test.local").expect("server name");
    tokio::time::pause();

    let (outcome, elapsed) = timed(handshake(&connector, name, tcp)).await;

    assert_timed_out(outcome);
    assert_took(elapsed, DIAL_STALL);
}
