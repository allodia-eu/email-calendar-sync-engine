//! Offline tests for the IMAP snapshot/delta UID-window paging, over a mock stream.

use super::*;
use crate::cursor::page_high;
use crate::mock::{MockStream, script, written};
use engine_provider::SyncKind;

/// Opens a connection over `server` and logs in (consuming the greeting + `a1`).
async fn logged_in(server: Vec<u8>) -> Connection<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn
}

/// A `SELECT` response advertising a UID space and message count.
fn select_resp(tag: &str, validity: u32, uid_next: u32, exists: u32) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n\
         * OK [UIDNEXT {uid_next}] n\r\n{tag} OK [READ-WRITE] done\r\n"
    )
}

/// A `FETCH` response with one row per UID (each seen, with a tiny envelope).
fn fetch_resp(tag: &str, uids: &[u32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (index, uid) in uids.iter().enumerate() {
        let seq = index + 1;
        write!(
            out,
            "* {seq} FETCH (UID {uid} FLAGS (\\Seen) \
             INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 10 \
             ENVELOPE (NIL \"s{uid}\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\"))\r\n"
        )
        .unwrap();
    }
    write!(out, "{tag} OK FETCH done\r\n").unwrap();
    out
}

fn inbox() -> MailboxId {
    MailboxId::try_from("INBOX").unwrap()
}

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

#[tokio::test]
async fn first_sync_snapshots_a_uid_window_newest_first() {
    // 8 messages, UIDNEXT 9 (UIDs 1..=8). With limit 3 the first window is the
    // newest three: 6:8.
    let select = select_resp("a2", 1000, 9, 8);
    let fetch = fetch_resp("a3", &[6, 7, 8]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 3).await.unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.total, Some(8));
    assert_eq!(page.changed.len(), 3);
    // Newest-first within the page.
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u8@INBOX");
    assert_eq!(page.changed[2].id.as_str(), "imap:v1000:u6@INBOX");
    assert_eq!(page.present.len(), 3);
    assert!(page.removed.is_empty());
    assert_eq!(page.next_cursor.as_str(), "v1000;n9");
    // The next window ends just below this one.
    assert_eq!(page_high(page.next_page.as_ref().unwrap()), Some(5));
    // The client fetched exactly the newest window.
    assert!(
        written(&recorded).contains("UID FETCH 6:8 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)")
    );
}

#[tokio::test]
async fn a_page_fills_to_the_limit_over_uid_gaps() {
    // 8 messages but the UID space is sparse near the top: UID 9 is a gap (UIDNEXT
    // runs ahead to 10). A limit-3 page must still return 3 *messages*, widening the
    // window downward over the gap instead of under-filling to 2.
    let select = select_resp("a2", 1000, 10, 8);
    let fetch_top = fetch_resp("a3", &[7, 8]); // window 7:9 → only 7,8 (9 is a gap)
    let fetch_next = fetch_resp("a4", &[4, 5, 6]); // widened window 4:6
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch_top, &fetch_next]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 3).await.unwrap();
    // Filled to the limit despite the gap, newest-first.
    assert_eq!(page.changed.len(), 3);
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u8@INBOX");
    assert_eq!(page.changed[1].id.as_str(), "imap:v1000:u7@INBOX");
    assert_eq!(page.changed[2].id.as_str(), "imap:v1000:u6@INBOX");
    assert_eq!(page.present.len(), 3);
    // The next page resumes just below the lowest kept UID (6).
    assert_eq!(page_high(page.next_page.as_ref().unwrap()), Some(5));
    // The client widened the window: it fetched 7:9, then 4:6.
    let sent = written(&recorded);
    assert!(sent.contains("UID FETCH 7:9"), "{sent}");
    assert!(sent.contains("UID FETCH 4:6"), "{sent}");
}

#[tokio::test]
async fn a_continuation_page_fetches_the_next_window_down() {
    let select = select_resp("a2", 1000, 10, 8);
    let fetch = fetch_resp("a3", &[4, 5, 6]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    // Resume from boundary 6 (what the first page handed back) with limit 3 → 4:6.
    let token = crate::cursor::page_token(6);
    let page = sync_page(&mut conn, &inbox(), None, Some(&token), 3)
        .await
        .unwrap();
    assert_eq!(page.changed.len(), 3);
    assert_eq!(page_high(page.next_page.as_ref().unwrap()), Some(3));
    assert!(written(&recorded).contains("UID FETCH 4:6"));
}

#[tokio::test]
async fn a_delta_with_no_new_arrivals_is_a_single_empty_page() {
    // Same UIDVALIDITY, UIDNEXT unchanged at 10 → nothing at or above the watermark.
    let select = select_resp("a2", 1000, 10, 8);
    let server = script(&[GREETING, LOGIN_OK, &select]);
    let mut conn = logged_in(server).await;

    let cursor = SyncState::new("v1000;n10");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.changed.is_empty());
    assert!(page.next_page.is_none());
    assert_eq!(page.next_cursor.as_str(), "v1000;n10");
}

#[tokio::test]
async fn a_delta_fetches_only_new_arrivals() {
    // UIDNEXT advanced 5 → 8: new UIDs 5,6,7.
    let select = select_resp("a2", 1000, 8, 7);
    let fetch = fetch_resp("a3", &[5, 6, 7]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let cursor = SyncState::new("v1000;n5");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert_eq!(page.changed.len(), 3);
    assert!(page.present.is_empty(), "a delta carries no present set");
    assert!(page.removed.is_empty());
    assert_eq!(page.next_cursor.as_str(), "v1000;n8");
    // Fetched the new-arrival window, not from UID 1.
    assert!(written(&recorded).contains("UID FETCH 5:7"));
}

#[tokio::test]
async fn a_uidvalidity_reset_forces_a_snapshot() {
    // The cursor's validity (111) no longer matches the server's (222): the UID
    // space was renumbered, so the whole mailbox is rediscovered as a snapshot.
    let select = select_resp("a2", 222, 4, 3);
    let fetch = fetch_resp("a3", &[1, 2, 3]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let mut conn = logged_in(server).await;

    let stale = SyncState::new("v111;n9");
    let page = sync_page(&mut conn, &inbox(), Some(&stale), None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 3);
    // Keys embed the NEW validity, so every old-validity row tombstones.
    assert_eq!(page.changed[0].id.as_str(), "imap:v222:u3@INBOX");
    assert_eq!(page.present.len(), 3);
    assert_eq!(page.next_cursor.as_str(), "v222;n4");
}

#[tokio::test]
async fn uid_next_is_derived_when_the_server_omits_it() {
    // SELECT advertises no UIDNEXT, so the client fetches the highest UID (`*`) to
    // derive the watermark before paging.
    let select = "* 3 EXISTS\r\n* OK [UIDVALIDITY 100] v\r\na2 OK [READ-WRITE] done\r\n";
    let derive = "* 3 FETCH (UID 3)\r\na3 OK FETCH done\r\n";
    let fetch = fetch_resp("a4", &[1, 2, 3]);
    let server = script(&[GREETING, LOGIN_OK, select, derive, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 3);
    // Derived UIDNEXT = highest UID (3) + 1.
    assert_eq!(page.next_cursor.as_str(), "v100;n4");
    assert!(written(&recorded).contains("UID FETCH * (UID)"));
}

#[tokio::test]
async fn an_empty_mailbox_snapshots_to_nothing() {
    let select = select_resp("a2", 1000, 1, 0);
    let server = script(&[GREETING, LOGIN_OK, &select]);
    let mut conn = logged_in(server).await;

    let page = sync_page(&mut conn, &inbox(), None, None, 50)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert!(page.changed.is_empty());
    // An empty present set tombstones every local row — the mailbox was emptied.
    assert!(page.present.is_empty());
    assert_eq!(page.total, Some(0));
    assert_eq!(page.next_cursor.as_str(), "v1000;n1");
}
