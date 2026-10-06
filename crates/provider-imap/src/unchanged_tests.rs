//! Skipping the reconciling delta for a folder that has not changed, on a server whose
//! `HIGHESTMODSEQ` moves on every change without QRESYNC. The capabilities and `SELECT` are
//! Yahoo's shape.

use engine_core::{ids::MailboxId, sync::SyncState};
use engine_provider::SyncKind;

use crate::{
    capability::Negotiated,
    cursor::MailboxCursor,
    mock::{MockStream, Recorded, script, written},
    resync::unchanged,
    sync::sync_page,
    transport::Connection,
};

const YAHOO: &[&str] = &["IMAP4rev1", "XYMHIGHESTMODSEQ", "MESSAGELIMIT=1000"];

fn select(uid_next: u32, modseq: u64) -> String {
    format!(
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 1000] v\r\n* OK [UIDNEXT {uid_next}] n\r\n\
         * OK [HIGHESTMODSEQ {modseq}]\r\na1 OK [READ-WRITE] done\r\n"
    )
}

async fn delta(
    capabilities: &[&str],
    replies: &[&str],
) -> (
    engine_provider::SyncPage<engine_core::mail::Message>,
    Recorded,
) {
    let mut server = vec!["* OK ready\r\n"];
    server.extend_from_slice(replies);
    let (stream, recorded) = MockStream::new(script(&server));
    let mut conn = Connection::open(stream).await.unwrap();
    let capabilities: Vec<String> = capabilities.iter().map(|&c| c.to_owned()).collect();
    conn.negotiated = Negotiated::from_capabilities(&capabilities);
    let cursor = SyncState::new("v1000;n10;m7");
    let inbox = MailboxId::try_from("INBOX").unwrap();
    let page = sync_page(&mut conn, &inbox, Some(&cursor), None, 0, None)
        .await
        .unwrap();
    (page, recorded)
}

#[tokio::test]
async fn an_unchanged_folder_costs_its_select() {
    let (page, recorded) = delta(YAHOO, &[&select(10, 7)]).await;

    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.changed.is_empty() && page.patched.is_empty() && page.present.is_empty());
    assert_eq!(page.next_cursor.as_str(), "v1000;n10;m7");
    assert!(
        !written(&recorded).contains("FETCH"),
        "{}",
        written(&recorded)
    );
}

#[tokio::test]
async fn a_moved_modseq_reads_the_folder() {
    let flags = "* 1 FETCH (FLAGS (\\Flagged) UID 2)\r\na2 OK done\r\n";
    let (page, recorded) = delta(YAHOO, &[&select(10, 8), flags]).await;

    assert_eq!(page.kind, SyncKind::Snapshot, "the reconciling delta ran");
    assert_eq!(page.patched.len(), 1);
    assert_eq!(page.next_cursor.as_str(), "v1000;n10;m8");
    assert!(written(&recorded).contains("a2 UID FETCH 1:9 (UID FLAGS)"));
}

#[tokio::test]
async fn a_server_that_does_not_say_its_modseq_tracks_every_change_is_read() {
    // Same SELECT, but nothing says the value moves on every change, so it is not trusted.
    let flags = "* 1 FETCH (FLAGS () UID 2)\r\na2 OK done\r\n";
    let (page, _) = delta(&["IMAP4rev1"], &[&select(10, 7), flags]).await;
    assert_eq!(page.kind, SyncKind::Snapshot);
}

#[test]
fn new_mail_or_a_missing_modseq_is_a_change() {
    let yahoo =
        Negotiated::from_capabilities(&YAHOO.iter().map(|&c| c.to_owned()).collect::<Vec<_>>());
    let prior = MailboxCursor::decode(&SyncState::new("v1000;n10;m7")).unwrap();
    assert!(unchanged(&yahoo, &prior, 10, Some(7)));
    assert!(!unchanged(&yahoo, &prior, 11, Some(7)));
    assert!(!unchanged(&yahoo, &prior, 10, None));
    let without = MailboxCursor::decode(&SyncState::new("v1000;n10")).unwrap();
    assert!(!unchanged(&yahoo, &without, 10, Some(7)));
}
