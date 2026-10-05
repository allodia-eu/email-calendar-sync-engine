//! An IMAP server that goes silent after the greeting: mid-command, mid-response, while an
//! `APPEND` literal goes out, and while a body arrives slowly but steadily.
//!
//! On tokio's paused clock, under a guard that turns a missing bound into a failure rather
//! than a hang, as in `deadline_tests`. Every transport here is in memory, so the paused
//! clock measures exactly.

use std::time::Duration;

use engine_core::error::FailureClass;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex},
    time::Instant,
};

use super::BOUNDS;
use crate::{
    error::{ImapError, ImapResult},
    mock::{MockStream, script, written},
    transport::Connection,
};

/// Longer than any bound under test, so only a missing bound reaches it.
const GUARD: Duration = Duration::from_hours(1);

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

/// A session past its greeting whose server answers `replies` and then nothing.
fn session(replies: &[&str]) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::silent_after(script(replies));
    (Connection::resume(stream), recorded)
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_after_any_command_fails_retryable_after_the_reply_bound() {
    // One of each kind of command a session sends: signing in, negotiating, opening a
    // mailbox, listing, searching, fetching, and the writes. None of them may wait on a
    // server that has stopped answering for longer than any other.
    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.login("alice", "pw")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.capabilities()).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.start_tls()).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.select("INBOX")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.list()).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.uid_search("ALL")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.uid_fetch("1:*", "UID FLAGS")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.uid_store("1", "+FLAGS.SILENT (\\Seen)")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.uid_move("1", "Archive")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());

    let (mut c, _) = session(&[]);
    let (outcome, elapsed) = timed(c.create("Sent")).await;
    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());
}

#[tokio::test(start_paused = true)]
async fn a_server_silent_partway_through_a_response_fails_after_the_reply_bound() {
    // The server has begun to answer and then stops before the completion that ends it.
    let (mut c, _) = session(&["* LIST (\\HasNoChildren) \"/\" INBOX\r\n"]);

    let (outcome, elapsed) = timed(c.list()).await;

    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());
}

#[tokio::test(start_paused = true)]
async fn an_append_the_server_never_invites_fails_retryable() {
    let (mut c, recorded) = session(&[]);

    let (outcome, elapsed) = timed(c.append("Drafts", "\\Draft", b"a message\r\n")).await;

    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());
    assert!(
        !written(&recorded).contains("a message"),
        "nothing was filed"
    );
}

#[tokio::test(start_paused = true)]
async fn an_append_whose_literal_goes_unanswered_fails_retryable() {
    let (mut c, recorded) = session(&["+ Ready for literal data\r\n"]);

    let (outcome, elapsed) = timed(c.append("Sent", "\\Seen", b"a message\r\n")).await;

    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.reply());
    assert!(written(&recorded).contains("a message"));
}

#[tokio::test(start_paused = true)]
async fn an_append_the_server_stops_taking_fails_after_the_stall_bound() {
    // The server invites the literal and then reads nothing, so the message fills the pipe.
    let (client, mut server) = duplex(1024);
    server
        .write_all(b"+ Ready for literal data\r\n")
        .await
        .expect("invitation");
    let mut c = Connection::resume(client);
    let message = "a line of the message\r\n".repeat(10_000);

    let (outcome, elapsed) = timed(c.append("Sent", "\\Seen", message.as_bytes())).await;

    let err = assert_timed_out(outcome);
    assert!(err.to_string().contains("took nothing"), "{err}");
    assert_took(elapsed, BOUNDS.stall());
    drop(server);
}

#[tokio::test(start_paused = true)]
async fn a_body_that_keeps_arriving_is_never_cut_off() {
    // Twenty pieces, one every fifty seconds: never silent for a stall, and in all far longer
    // than any one bound.
    const PIECE: usize = 1000;
    const PIECES: usize = 20;
    let (client, server) = duplex(64 * 1024);
    let serving = tokio::spawn(async move {
        let mut server = BufReader::new(server);
        let mut command = String::new();
        server.read_line(&mut command).await.expect("command");
        let tag = command.split(' ').next().expect("tag").to_owned();
        let head = format!("* 1 FETCH (UID 7 BODY[] {{{}}}\r\n", PIECE * PIECES);
        server.write_all(head.as_bytes()).await.expect("head");
        for _ in 0..PIECES {
            tokio::time::sleep(Duration::from_secs(50)).await;
            server.write_all(&[b'x'; PIECE]).await.expect("piece");
        }
        let end = format!(")\r\n{tag} OK done\r\n");
        server.write_all(end.as_bytes()).await.expect("completion");
        server
    });
    let mut c = Connection::resume(client);

    let (outcome, elapsed) = timed(c.uid_fetch_body(7)).await;

    let body = outcome.expect("a steady body arrives").expect("a body");
    assert_eq!(body.len(), PIECE * PIECES);
    assert!(elapsed > BOUNDS.submission(), "{elapsed:?}");
    drop(serving.await.expect("server"));
}

#[tokio::test(start_paused = true)]
async fn a_body_that_stops_fails_after_the_stall_bound() {
    let (mut c, _) = session(&["* 1 FETCH (UID 7 BODY[] {100}\r\nonly part of it"]);

    let (outcome, elapsed) = timed(c.uid_fetch_body(7)).await;

    assert_timed_out(outcome);
    assert_took(elapsed, BOUNDS.stall());
}
