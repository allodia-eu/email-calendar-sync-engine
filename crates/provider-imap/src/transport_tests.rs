//! Offline tests for the IMAP line protocol, driven over a scripted mock stream.

use super::*;
use crate::mock::{MockStream, script, written};
use engine_core::error::FailureClass;

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] Stalwart ready\r\n";

#[tokio::test]
async fn a_full_session_drives_commands_and_parses_responses() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* 8 EXISTS\r\n* OK [UIDVALIDITY 1234567890] valid\r\n\
         * OK [UIDNEXT 10] next\r\na2 OK [READ-WRITE] SELECT done\r\n",
        "* 1 FETCH (UID 1 FLAGS (\\Seen) ENVELOPE \
         (\"d\" \"Hello\" ((\"A\" NIL \"a\" \"h\")) NIL NIL NIL NIL NIL NIL \"<m@h>\"))\r\n\
         a3 OK FETCH done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);

    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice@test.local", "pw").await.unwrap();

    let select = conn.select("INBOX").await.unwrap();
    assert_eq!(select.uid_validity, 1_234_567_890);
    assert_eq!(select.uid_next, Some(10));
    assert_eq!(select.exists, 8);

    let rows = conn.uid_fetch("1:8", "UID FLAGS ENVELOPE").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uid, 1);
    assert!(rows[0].flags.iter().any(|f| f == r"\Seen"));
    assert_eq!(
        rows[0].envelope.as_ref().unwrap().subject.as_deref(),
        Some("Hello")
    );

    // The client issued exactly the expected, correctly-tagged commands.
    let sent = written(&recorded);
    assert!(
        sent.contains("a1 LOGIN \"alice@test.local\" \"pw\""),
        "{sent}"
    );
    assert!(sent.contains("a2 SELECT \"INBOX\""), "{sent}");
    assert!(
        sent.contains("a3 UID FETCH 1:8 (UID FLAGS ENVELOPE)"),
        "{sent}"
    );
}

#[tokio::test]
async fn login_failure_maps_to_authentication() {
    let server = script(&[GREETING, "a1 NO [AUTHENTICATIONFAILED] bad credentials\r\n"]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();

    let err = conn.login("alice@test.local", "wrong").await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Authentication);
}

#[tokio::test]
async fn a_bye_greeting_is_a_retryable_error() {
    let (stream, _) = MockStream::new(script(&["* BYE server too busy\r\n"]));
    let err = Connection::open(stream).await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Retryable);
}

#[tokio::test]
async fn negotiate_qresync_enables_it_when_advertised() {
    // CAPABILITY advertises QRESYNC (post-auth, as Stalwart does), so the client
    // ENABLEs it and the session is QRESYNC-aware.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 ENABLE CONDSTORE QRESYNC UIDPLUS\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED QRESYNC\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    assert!(!conn.qresync_enabled());

    conn.negotiate_qresync().await.unwrap();
    assert!(conn.qresync_enabled());
    // This server advertises QRESYNC but not IDLE, so no push capability is recorded.
    assert!(!conn.idle_advertised());

    let sent = written(&recorded);
    assert!(sent.contains("a2 CAPABILITY"), "{sent}");
    assert!(sent.contains("a3 ENABLE QRESYNC"), "{sent}");
}

#[tokio::test]
async fn negotiate_qresync_stays_off_without_the_capability() {
    // No QRESYNC advertised → no ENABLE issued, the session stays on the baseline.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 IDLE UIDPLUS\r\na2 OK CAPABILITY done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate_qresync().await.unwrap();
    assert!(!conn.qresync_enabled());
    // The same post-auth CAPABILITY lists IDLE (but not QRESYNC), so push is recorded
    // even though the QRESYNC delta is not — the two are independent capabilities.
    assert!(conn.idle_advertised());
    assert!(!written(&recorded).contains("ENABLE"), "no ENABLE sent");
}

#[tokio::test]
async fn negotiate_qresync_ignores_an_ok_that_enabled_nothing() {
    // The server advertises QRESYNC and answers ENABLE with a tagged OK, but the
    // `* ENABLED` line is empty (it enabled nothing, RFC 5161) — so we must stay on the
    // baseline rather than issue CONDSTORE/VANISHED commands the server would reject.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "* ENABLED\r\na3 OK ENABLE successful\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate_qresync().await.unwrap();
    assert!(
        !conn.qresync_enabled(),
        "a bare ENABLED list does not enable QRESYNC"
    );
}

#[tokio::test]
async fn negotiate_qresync_tolerates_an_enable_rejection() {
    // A server that advertises QRESYNC but rejects ENABLE leaves the session on the
    // baseline rather than failing the whole connection.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "a3 NO ENABLE not right now\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    conn.negotiate_qresync().await.unwrap();
    assert!(
        !conn.qresync_enabled(),
        "an ENABLE NO falls back to baseline"
    );
}

#[tokio::test]
async fn negotiate_qresync_propagates_a_transport_error() {
    // A non-NO/BAD failure during ENABLE (here a protocol violation) is propagated,
    // not swallowed — only an advertised-but-refused NO/BAD falls back to baseline.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* CAPABILITY IMAP4rev2 QRESYNC\r\na2 OK CAPABILITY done\r\n",
        "+ unexpected continuation\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let err = conn.negotiate_qresync().await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(!conn.qresync_enabled());
}

#[tokio::test]
async fn select_condstore_reads_highest_modseq() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* 8 EXISTS\r\n* OK [UIDVALIDITY 347529756] valid\r\n\
         * OK [UIDNEXT 10] next\r\n* OK [HIGHESTMODSEQ 16] modseq\r\n\
         a2 OK [READ-WRITE] SELECT done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let select = conn.select_condstore("INBOX").await.unwrap();
    assert_eq!(select.highest_modseq, Some(16));
    assert!(
        written(&recorded).contains("a2 SELECT \"INBOX\" (CONDSTORE)"),
        "the SELECT requests CONDSTORE"
    );
}

#[tokio::test]
async fn uid_fetch_changedsince_returns_rows_and_vanished() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN completed\r\n",
        "* VANISHED (EARLIER) 7\r\n\
         * 2 FETCH (UID 2 FLAGS (\\Flagged \\Seen) MODSEQ (24))\r\n\
         a2 OK UID FETCH completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let (rows, vanished) = conn
        .uid_fetch_changedsince("1:*", "UID FLAGS", 16)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uid, 2);
    assert_eq!(vanished, [7]);
    assert!(
        written(&recorded).contains("a2 UID FETCH 1:* (UID FLAGS) (CHANGEDSINCE 16 VANISHED)"),
        "the fetch carries CHANGEDSINCE and VANISHED"
    );
}

#[tokio::test]
async fn a_select_no_is_an_invalid_state_error() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "a2 NO [NONEXISTENT] mailbox does not exist\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let err = conn.select("Missing").await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn fetch_reassembles_a_literal_across_lines() {
    // The ENVELOPE subject arrives as a `{7}` literal the transport must inline.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "* 1 FETCH (UID 5 ENVELOPE (NIL {7}\r\nSubject NIL NIL NIL NIL NIL NIL NIL NIL))\r\n\
         a2 OK FETCH done\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let rows = conn.uid_fetch("5", "ENVELOPE").await.unwrap();
    assert_eq!(rows[0].uid, 5);
    assert_eq!(
        rows[0].envelope.as_ref().unwrap().subject.as_deref(),
        Some("Subject")
    );
}

#[tokio::test]
async fn connection_has_a_debug_repr() {
    let (stream, _) = MockStream::new(script(&[GREETING]));
    let conn = Connection::open(stream).await.unwrap();
    assert!(format!("{conn:?}").contains("Connection"));
}

#[tokio::test]
async fn a_preauth_greeting_opens_without_login() {
    let (stream, _) = MockStream::new(script(&["* PREAUTH already authenticated\r\n"]));
    // Opening succeeds: a PREAUTH greeting is a valid (pre-authenticated) session.
    assert!(Connection::open(stream).await.is_ok());
}

#[tokio::test]
async fn append_without_a_continuation_is_a_protocol_error() {
    // The server rejects APPEND outright instead of asking for the literal.
    let server = script(&[GREETING, "a1 OK LOGIN ok\r\n", "a2 NO mailbox is full\r\n"]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    assert!(conn.append("Sent", "\\Seen", b"x\r\n").await.is_err());
}

#[tokio::test]
async fn create_issues_the_command() {
    let server = script(&[GREETING, "a1 OK LOGIN ok\r\n", "a2 OK CREATE completed\r\n"]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    conn.create("Sent").await.unwrap();
    assert!(written(&recorded).contains("a2 CREATE \"Sent\""));
}

#[tokio::test]
async fn append_sends_a_literal_and_parses_appenduid() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "+ OK send the literal\r\n",
        "a2 OK [APPENDUID 99 7] APPEND completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let message = b"Subject: x\r\n\r\nhi\r\n"; // 18 bytes
    let uid = conn.append("Sent", "\\Seen", message).await.unwrap();
    assert_eq!(uid, Some((99, 7)));

    let sent = written(&recorded);
    assert!(sent.contains("a2 APPEND \"Sent\" (\\Seen) {18}"), "{sent}");
    assert!(sent.contains("Subject: x"));
}

#[tokio::test]
async fn list_returns_every_mailbox() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
         * LIST (\\HasNoChildren \\Sent) \"/\" \"Sent\"\r\n\
         a2 OK LIST done\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let rows = conn.list().await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "INBOX");
    assert_eq!(rows[1].name, "Sent");
    assert!(written(&recorded).contains("a2 LIST \"\" \"*\""));
}

#[tokio::test]
async fn a_huge_announced_literal_is_rejected_before_allocating() {
    // A hostile server announces an enormous literal; the transport must reject it
    // (bounding the allocation) rather than try to read ~100 MB into memory.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "* 1 FETCH (UID 1 ENVELOPE (NIL {99999999}\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let err = conn.uid_fetch("1", "ENVELOPE").await.unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[tokio::test]
async fn uid_store_issues_a_silent_flag_command() {
    let server = script(&[GREETING, "a1 OK LOGIN ok\r\n", "a2 OK STORE completed\r\n"]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    conn.uid_store("42", "+FLAGS.SILENT (\\Seen)")
        .await
        .unwrap();
    assert!(
        written(&recorded).contains("a2 UID STORE 42 +FLAGS.SILENT (\\Seen)"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn uid_store_no_is_an_invalid_state_error() {
    // A tagged NO (e.g. the message vanished) surfaces as InvalidState, not success.
    let server = script(&[GREETING, "a1 OK LOGIN ok\r\n", "a2 NO no such message\r\n"]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    let err = conn
        .uid_store("99", "+FLAGS.SILENT (\\Deleted)")
        .await
        .unwrap_err();
    assert_eq!(err.failure_class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn uid_move_quotes_the_destination() {
    let server = script(&[GREETING, "a1 OK LOGIN ok\r\n", "a2 OK MOVE completed\r\n"]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    conn.uid_move("42", "Archive").await.unwrap();
    assert!(
        written(&recorded).contains("a2 UID MOVE 42 \"Archive\""),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn uid_expunge_issues_the_command() {
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "a2 OK EXPUNGE completed\r\n",
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();
    conn.uid_expunge("42").await.unwrap();
    assert!(
        written(&recorded).contains("a2 UID EXPUNGE 42"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn append_tolerates_an_untagged_response_before_the_continuation() {
    // A server may interleave an unsolicited untagged response (here `* 5 EXISTS`)
    // before the `+` continuation; APPEND must skip it, not fail.
    let server = script(&[
        GREETING,
        "a1 OK LOGIN ok\r\n",
        "* 5 EXISTS\r\n+ OK send the literal\r\n",
        "a2 OK [APPENDUID 99 7] APPEND completed\r\n",
    ]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("a", "b").await.unwrap();

    let uid = conn
        .append("Sent", "\\Seen", b"Subject: x\r\n\r\nhi\r\n")
        .await
        .unwrap();
    assert_eq!(uid, Some((99, 7)));
}
