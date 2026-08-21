//! Offline tests for [`report_message`], driven over a scripted mock stream.
//!
//! The `SELECT` responses below are the bytes Stalwart really sent (captured from the
//! harness on 2026-08-21), including the `PERMANENTFLAGS` line — the one this path
//! reads before it writes.

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};
use engine_provider::{MessageReport, ReportVerdict};

use super::report_message;
use crate::{
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// Stalwart's real `SELECT INBOX` reply, verbatim but for the `UIDVALIDITY` (matched
/// to the keys below). Note `\*` at the end of `PERMANENTFLAGS`.
const SELECT_ALLOWS_KEYWORDS: &str = "* 8 EXISTS\r\n\
     * FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
     * 0 RECENT\r\n\
     * OK [PERMANENTFLAGS (\\Deleted \\Seen \\Answered \\Flagged \\Draft \\*)] All allowed\r\n\
     * OK [UIDVALIDITY 7] UIDs valid\r\n\
     * OK [UIDNEXT 21] Next predicted UID\r\n\
     a2 OK [READ-WRITE] SELECT completed\r\n";

/// The same reply from a server that does **not** permit new keywords — the `\*` is
/// gone. No server we run behaves this way, which is exactly why it is scripted here.
const SELECT_FIXED_FLAGS: &str = "* 8 EXISTS\r\n\
     * FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
     * OK [PERMANENTFLAGS (\\Deleted \\Seen \\Answered \\Flagged \\Draft)] Limited\r\n\
     * OK [UIDVALIDITY 7] UIDs valid\r\n\
     a2 OK [READ-WRITE] SELECT completed\r\n";

fn target() -> ProviderKey {
    ProviderKey::new("imap:v7:u42@INBOX").unwrap()
}

fn report(verdict: ReportVerdict, destination: &str) -> MessageReport {
    MessageReport::new(target(), verdict, MailboxId::try_from(destination).unwrap())
}

async fn logged_in(server: Vec<u8>) -> (Connection<MockStream>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

#[tokio::test]
async fn a_junk_report_stores_the_keyword_clears_its_opposite_then_moves() {
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_ALLOWS_KEYWORDS,
        "a3 OK STORE done\r\n",
        "a4 OK STORE done\r\n",
        "a5 OK MOVE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    let receipt = report_message(&mut conn, &report(ReportVerdict::Junk, "Junk"))
        .await
        .unwrap();
    // The move mints a new UID server-side, so the receipt names the source key and the
    // destination reconciles on its next sync.
    assert_eq!(receipt.message_key, target());

    let sent = written(&recorded);
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT ($Junk)"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID STORE 42 -FLAGS.SILENT ($NotJunk)"),
        "{sent}"
    );
    assert!(sent.contains("a5 UID MOVE 42 \"Junk\""), "{sent}");
}

#[tokio::test]
async fn a_not_junk_report_is_the_inverse_and_files_back_to_the_inbox() {
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_ALLOWS_KEYWORDS,
        "a3 OK STORE done\r\n",
        "a4 OK STORE done\r\n",
        "a5 OK MOVE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    report_message(&mut conn, &report(ReportVerdict::NotJunk, "Archive"))
        .await
        .unwrap();

    let sent = written(&recorded);
    assert!(
        sent.contains("a3 UID STORE 42 +FLAGS.SILENT ($NotJunk)"),
        "{sent}"
    );
    assert!(
        sent.contains("a4 UID STORE 42 -FLAGS.SILENT ($Junk $Phishing)"),
        "{sent}"
    );
}

#[tokio::test]
async fn phishing_stores_its_own_keyword() {
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_ALLOWS_KEYWORDS,
        "a3 OK STORE done\r\n",
        "a4 OK STORE done\r\n",
        "a5 OK MOVE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    report_message(&mut conn, &report(ReportVerdict::Phishing, "Junk"))
        .await
        .unwrap();
    assert!(
        written(&recorded).contains("a3 UID STORE 42 +FLAGS.SILENT ($Phishing)"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_report_into_the_message_s_own_mailbox_does_not_move_it() {
    // Reporting something that already sits in Junk should still set the keyword, but
    // `UID MOVE` onto the selected mailbox is not a no-op on every server.
    let server = script(&[
        GREETING,
        LOGIN_OK,
        SELECT_ALLOWS_KEYWORDS,
        "a3 OK STORE done\r\n",
        "a4 OK STORE done\r\n",
    ]);
    let (mut conn, recorded) = logged_in(server).await;

    report_message(&mut conn, &report(ReportVerdict::Junk, "INBOX"))
        .await
        .unwrap();

    let sent = written(&recorded);
    assert!(sent.contains("+FLAGS.SILENT ($Junk)"), "{sent}");
    assert!(!sent.contains("UID MOVE"), "should not move: {sent}");
}

#[tokio::test]
async fn a_server_that_forbids_new_keywords_is_refused_not_silently_dropped() {
    // The whole point of parsing PERMANENTFLAGS: this server would answer the STORE
    // with a plain OK and keep nothing.
    let server = script(&[GREETING, LOGIN_OK, SELECT_FIXED_FLAGS]);
    let (mut conn, recorded) = logged_in(server).await;

    let err = report_message(&mut conn, &report(ReportVerdict::Junk, "Junk"))
        .await
        .expect_err("a server without \\* must refuse the report");
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(
        err.detail().contains("PERMANENTFLAGS"),
        "the error should name what it read: {}",
        err.detail()
    );

    // And it must refuse *before* writing anything.
    let sent = written(&recorded);
    assert!(!sent.contains("UID STORE"), "{sent}");
    assert!(!sent.contains("UID MOVE"), "{sent}");
}

#[tokio::test]
async fn a_stale_uidvalidity_is_a_conflict() {
    let stale = "* 8 EXISTS\r\n\
         * OK [PERMANENTFLAGS (\\Seen \\*)] ok\r\n\
         * OK [UIDVALIDITY 99] moved on\r\n\
         a2 OK [READ-WRITE] done\r\n";
    let server = script(&[GREETING, LOGIN_OK, stale]);
    let (mut conn, _recorded) = logged_in(server).await;

    let err = report_message(&mut conn, &report(ReportVerdict::Junk, "Junk"))
        .await
        .expect_err("a renumbered UID space must not be written blind");
    assert_eq!(err.class(), FailureClass::Conflict);
}
