//! Offline tests for the IDLE (RFC 2177) transport primitives, driven over the
//! scripted [`MockStream`]. These cover the non-blocking paths (a complete transcript
//! is available, or the connection drops); the keep-alive *timing* and the
//! stay-idling-across-events behavior are exercised at the watcher level over a real
//! in-memory duplex (`crate::watch`).

use super::{IdleLine, classify, idle_done, idle_start, idle_wait_change};
use crate::error::ImapError;
use crate::mock::{MockStream, Recorded, script, written};
use crate::transport::Connection;

const GREETING: &str = "* OK ready\r\n";

/// Opens a connection over `GREETING` + the supplied IDLE-phase server bytes, so a
/// primitive can be driven directly (greeting consumed, next tag is `a1`).
async fn idle_conn(server: &[&str]) -> (Connection<MockStream>, Recorded) {
    let mut parts = vec![GREETING];
    parts.extend_from_slice(server);
    let (stream, recorded) = MockStream::new(script(&parts));
    let conn = Connection::open(stream).await.unwrap();
    (conn, recorded)
}

#[tokio::test]
async fn idle_start_consumes_the_continuation_and_returns_the_tag() {
    let (mut conn, recorded) = idle_conn(&["+ idling\r\n"]).await;
    let tag = idle_start(&mut conn).await.unwrap();
    assert_eq!(tag, "a1");
    assert!(
        written(&recorded).contains("a1 IDLE\r\n"),
        "issues tagged IDLE"
    );
}

#[tokio::test]
async fn idle_start_skips_untagged_status_before_the_continuation() {
    // A server may interleave an untagged response before `+ idling`; it is tolerated.
    let (mut conn, _) = idle_conn(&["* 4 EXISTS\r\n+ idling\r\n"]).await;
    assert_eq!(idle_start(&mut conn).await.unwrap(), "a1");
}

#[tokio::test]
async fn idle_start_errors_when_the_server_refuses() {
    // A tagged completion instead of a continuation means IDLE was refused.
    let (mut conn, _) = idle_conn(&["a1 BAD IDLE not allowed\r\n"]).await;
    assert!(matches!(
        idle_start(&mut conn).await.unwrap_err(),
        ImapError::Protocol(_)
    ));
}

#[tokio::test]
async fn idle_wait_change_returns_on_each_change_notification() {
    for notif in [
        "* 5 EXISTS\r\n",
        "* 3 EXPUNGE\r\n",
        "* 2 FETCH (FLAGS (\\Seen))\r\n",
        "* VANISHED 9\r\n",
    ] {
        let (mut conn, _) = idle_conn(&[notif]).await;
        idle_wait_change(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("{notif:?} should be a change, got {e:?}"));
    }
}

#[tokio::test]
async fn idle_wait_change_skips_informational_then_returns_on_a_change() {
    // RECENT and a server "still here" poke are consumed; the EXISTS that follows
    // ends the wait.
    let (mut conn, _) = idle_conn(&["* 5 RECENT\r\n* OK Still here\r\n* 6 EXISTS\r\n"]).await;
    idle_wait_change(&mut conn).await.unwrap();
}

#[tokio::test]
async fn idle_wait_change_surfaces_a_bye() {
    let (mut conn, _) = idle_conn(&["* BYE server shutting down\r\n"]).await;
    assert!(matches!(
        idle_wait_change(&mut conn).await.unwrap_err(),
        ImapError::Bye(_)
    ));
}

#[tokio::test]
async fn idle_wait_change_errors_on_an_unexpected_tagged_line() {
    // A tagged line mid-IDLE (we never sent DONE) is a protocol violation.
    let (mut conn, _) = idle_conn(&["a1 OK out of band\r\n"]).await;
    assert!(matches!(
        idle_wait_change(&mut conn).await.unwrap_err(),
        ImapError::Protocol(_)
    ));
}

#[tokio::test]
async fn idle_wait_change_propagates_a_dropped_connection() {
    // An informational line then EOF (the socket died) surfaces as a transport error,
    // which the host treats as retryable and reconnects.
    let (mut conn, _) = idle_conn(&["* 5 RECENT\r\n"]).await;
    assert!(matches!(
        idle_wait_change(&mut conn).await.unwrap_err(),
        ImapError::Io(_)
    ));
}

#[tokio::test]
async fn idle_done_drains_and_detects_a_boundary_change() {
    // A change that arrives right as we end IDLE is reported, not swallowed.
    let (mut conn, recorded) = idle_conn(&["* 7 EXISTS\r\na9 OK IDLE terminated\r\n"]).await;
    assert!(idle_done(&mut conn, "a9").await.unwrap());
    assert!(written(&recorded).contains("DONE\r\n"), "issues bare DONE");
}

#[tokio::test]
async fn idle_done_reports_no_change_on_a_quiet_drain() {
    let (mut conn, _) = idle_conn(&["a9 OK terminated\r\n"]).await;
    assert!(!idle_done(&mut conn, "a9").await.unwrap());
}

#[tokio::test]
async fn idle_done_maps_no_and_bad_completions_to_errors() {
    let (mut conn, _) = idle_conn(&["a9 NO cannot end\r\n"]).await;
    assert!(matches!(
        idle_done(&mut conn, "a9").await.unwrap_err(),
        ImapError::No(_)
    ));

    let (mut conn, _) = idle_conn(&["a9 BAD malformed\r\n"]).await;
    assert!(matches!(
        idle_done(&mut conn, "a9").await.unwrap_err(),
        ImapError::Bad(_)
    ));

    // An unrecognized completion status is a protocol error, never silently OK.
    let (mut conn, _) = idle_conn(&["a9 HUH weird\r\n"]).await;
    assert!(matches!(
        idle_done(&mut conn, "a9").await.unwrap_err(),
        ImapError::Protocol(_)
    ));
}

#[tokio::test]
async fn idle_done_surfaces_a_bye_before_the_completion() {
    let (mut conn, _) = idle_conn(&["* BYE going away\r\n"]).await;
    assert!(matches!(
        idle_done(&mut conn, "a9").await.unwrap_err(),
        ImapError::Bye(_)
    ));
}

#[test]
fn classify_distinguishes_changes_informational_bye_and_tagged() {
    // Changes: the four notification kinds, case-insensitively.
    assert_eq!(classify(b"* 4 EXISTS\r\n"), IdleLine::Changed);
    assert_eq!(classify(b"* 3 EXPUNGE\r\n"), IdleLine::Changed);
    assert_eq!(
        classify(b"* 2 FETCH (FLAGS (\\Seen))\r\n"),
        IdleLine::Changed
    );
    assert_eq!(classify(b"* VANISHED (EARLIER) 1:3\r\n"), IdleLine::Changed);
    assert_eq!(classify(b"* 4 exists\r\n"), IdleLine::Changed);

    // Informational: RECENT, an OK poke, and any other untagged head.
    assert_eq!(classify(b"* 5 RECENT\r\n"), IdleLine::Informational);
    assert_eq!(classify(b"* OK Still here\r\n"), IdleLine::Informational);
    assert_eq!(classify(b"* FLAGS (\\Seen)\r\n"), IdleLine::Informational);

    // BYE and a tagged (non-untagged) line.
    assert_eq!(classify(b"* BYE later\r\n"), IdleLine::Bye);
    assert_eq!(classify(b"a1 OK done\r\n"), IdleLine::Unexpected);

    // Hostile/degenerate input never panics.
    assert_eq!(classify(b"* \r\n"), IdleLine::Informational);
    assert_eq!(classify(b""), IdleLine::Unexpected);
    assert_eq!(classify(b"* notanumber WORD\r\n"), IdleLine::Informational);
}
