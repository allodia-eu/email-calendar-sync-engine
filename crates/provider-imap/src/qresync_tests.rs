//! Offline tests for the QRESYNC incremental delta, replaying the **exact** bytes
//! captured from a live Stalwart `UID FETCH … (CHANGEDSINCE … VANISHED)` (an observed
//! provider transcript, per `providers.md`).

use super::*;
use crate::mock::{MockStream, script, written};
use engine_core::mail::SystemKeyword;
use engine_provider::SyncKind;

/// Opens a connection over `server` and logs in (consuming the greeting + `a1`).
async fn logged_in(server: Vec<u8>) -> Connection<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn
}

fn inbox() -> MailboxId {
    MailboxId::try_from("INBOX").unwrap()
}

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// The real Stalwart response to
/// `UID FETCH 1:* (… ) (CHANGEDSINCE 16 VANISHED)` after UID 2 was re-flagged and
/// UID 7 expunged: a `VANISHED (EARLIER)` line plus a full-metadata FETCH whose
/// `References` rides a `{2}` literal (the empty-header blank line) and which carries a
/// trailing `MODSEQ (24)` the parser ignores.
const CHANGEDSINCE_RESP: &str = "* VANISHED (EARLIER) 7\r\n\
     * 2 FETCH (UID 2 FLAGS (\\Flagged \\Seen) INTERNALDATE \"28-Jun-2026 09:34:39 +0000\" \
     RFC822.SIZE 449 ENVELOPE (\"Tue, 6 Jan 2026 10:00:00 +0000\" \
     \"Duplicate Message-ID (copy A)\" ((\"Newsletter\" NIL \"news\" \"example.com\")) \
     ((\"Newsletter\" NIL \"news\" \"example.com\")) ((\"Newsletter\" NIL \"news\" \"example.com\")) \
     ((\"Alice Tester\" NIL \"alice\" \"test.local\")) NIL NIL NIL \
     \"<shared-dup-msgid@example.com>\") BODY[HEADER.FIELDS (REFERENCES)] {2}\r\n\r\n \
     MODSEQ (24))\r\na2 OK UID FETCH completed\r\n";

#[tokio::test]
async fn a_qresync_delta_carries_flag_changes_and_vanished_removals() {
    let server = script(&[GREETING, LOGIN_OK, CHANGEDSINCE_RESP]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let next_cursor = SyncState::new("v347529756;n10;m26");
    let page = delta_page(&mut conn, &inbox(), 347_529_756, next_cursor.clone(), 16)
        .await
        .unwrap();

    // It is a delta, single page, carrying the modseq baseline forward.
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.next_page.is_none());
    assert!(page.present.is_empty(), "a delta carries no present set");
    assert_eq!(page.next_cursor, next_cursor);
    assert_eq!(page.total, None);

    // The flag-changed message comes back with full metadata and its new keywords.
    assert_eq!(page.changed.len(), 1);
    let changed = &page.changed[0];
    assert_eq!(changed.id.as_str(), "imap:v347529756:u2@INBOX");
    assert_eq!(
        changed.envelope.subject.as_deref(),
        Some("Duplicate Message-ID (copy A)")
    );
    assert!(changed.has_system_keyword(SystemKeyword::Flagged));
    assert!(changed.has_system_keyword(SystemKeyword::Seen));
    // The Message-ID hint survives so reconciliation/threading still works.
    assert!(
        changed
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == "shared-dup-msgid@example.com")
    );

    // The expunged UID 7 is a removal — the store tombstones it inline (no snapshot).
    assert_eq!(page.removed.len(), 1);
    assert_eq!(page.removed[0].as_str(), "imap:v347529756:u7@INBOX");

    // The one command carried the CHANGEDSINCE baseline and the VANISHED modifier.
    let sent = written(&recorded);
    assert!(
        sent.contains(
            "UID FETCH 1:* (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE \
             BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (REFERENCES)]) (CHANGEDSINCE 16 VANISHED)"
        ),
        "{sent}"
    );
}

#[tokio::test]
async fn a_qresync_delta_drops_unsolicited_flag_only_rows() {
    // The CHANGEDSINCE response carries a solicited full row for UID 2, plus an
    // *unsolicited* flag-only `* 9 FETCH (UID 9 FLAGS (..) MODSEQ (..))` (no ENVELOPE)
    // for a concurrently-changed message. Mapping that envelope-less row would upsert
    // an empty Message over UID 9's good metadata, so it must be dropped — only the
    // full row (UID 2) becomes a change.
    let resp = "* 2 FETCH (UID 2 FLAGS (\\Flagged \\Seen) \
         INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 20 \
         ENVELOPE (NIL \"real subject\" ((\"A\" NIL \"a\" \"h\")) NIL NIL NIL NIL NIL NIL \"<m2@h>\"))\r\n\
         * 9 FETCH (UID 9 FLAGS (\\Seen) MODSEQ (40))\r\n\
         a2 OK UID FETCH completed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n10;m40"),
        9,
    )
    .await
    .unwrap();
    assert_eq!(
        page.changed.len(),
        1,
        "the flag-only row for UID 9 is dropped"
    );
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u2@INBOX");
    assert_eq!(
        page.changed[0].envelope.subject.as_deref(),
        Some("real subject"),
        "the surviving row keeps its real metadata"
    );
}

#[tokio::test]
async fn a_qresync_delta_with_no_changes_is_empty() {
    // A `CHANGEDSINCE` that matched nothing: no FETCH rows, no VANISHED — a clean,
    // empty delta that still advances the cursor.
    let resp = "a2 OK UID FETCH completed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;

    let page = delta_page(&mut conn, &inbox(), 1000, SyncState::new("v1000;n5;m9"), 9)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.changed.is_empty());
    assert!(page.removed.is_empty());
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn a_qresync_delta_surfaces_a_fetch_error() {
    // A tagged NO on the CHANGEDSINCE fetch propagates as a classified error (not a
    // silent empty delta), so the orchestrator can reclassify/retry.
    let resp = "a2 NO [SERVERBUG] fetch failed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;
    let err = delta_page(&mut conn, &inbox(), 1000, SyncState::new("v1000;n5;m9"), 9)
        .await
        .unwrap_err();
    assert_eq!(
        err.failure_class(),
        engine_core::error::FailureClass::InvalidState
    );
}

#[tokio::test]
async fn a_vanished_range_expands_to_every_removed_key() {
    // QRESYNC may collapse a run of expunges into a `(EARLIER) 3:5,9` set; each UID
    // must become its own removal key.
    let resp = "* VANISHED (EARLIER) 3:5,9\r\na2 OK UID FETCH completed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n10;m20"),
        7,
    )
    .await
    .unwrap();
    let removed: Vec<&str> = page.removed.iter().map(ProviderKey::as_str).collect();
    assert_eq!(
        removed,
        [
            "imap:v1000:u3@INBOX",
            "imap:v1000:u4@INBOX",
            "imap:v1000:u5@INBOX",
            "imap:v1000:u9@INBOX",
        ]
    );
    assert!(page.changed.is_empty());
}
