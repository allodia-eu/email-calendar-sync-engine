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

/// A `FETCH` response with one row per UID (each seen, with a tiny envelope/bodystructure and
/// an empty echoed `References` header — what a server returns for the peek item).
fn fetch_resp(tag: &str, uids: &[u32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (index, uid) in uids.iter().enumerate() {
        let seq = index + 1;
        write!(
            out,
            "* {seq} FETCH (UID {uid} FLAGS (\\Seen) \
             INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 10 \
             ENVELOPE (NIL \"s{uid}\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\") \
             BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 2 1) \
             BODY[HEADER.FIELDS (REFERENCES)] \"\")\r\n"
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

    let page = sync_page(&mut conn, &inbox(), None, None, 3, None)
        .await
        .unwrap();
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
    // The client fetched exactly the newest window, including the References header.
    assert!(written(&recorded).contains(
        "UID FETCH 6:8 (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE \
         BODYSTRUCTURE BODY.PEEK[HEADER.FIELDS (REFERENCES)])"
    ));
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

    let page = sync_page(&mut conn, &inbox(), None, None, 3, None)
        .await
        .unwrap();
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
    let page = sync_page(&mut conn, &inbox(), None, Some(&token), 3, None)
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
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50, None)
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
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50, None)
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
    let page = sync_page(&mut conn, &inbox(), Some(&stale), None, 50, None)
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

    let page = sync_page(&mut conn, &inbox(), None, None, 50, None)
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

    let page = sync_page(&mut conn, &inbox(), None, None, 50, None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert!(page.changed.is_empty());
    // An empty present set tombstones every local row — the mailbox was emptied.
    assert!(page.present.is_empty());
    assert_eq!(page.total, Some(0));
    assert_eq!(page.next_cursor.as_str(), "v1000;n1");
}

#[tokio::test]
async fn a_windowed_snapshot_starts_at_the_lowest_in_window_uid() {
    // 8 messages (UIDs 1..=8), but only UIDs 5..=8 fall within the sync-depth window.
    // `UID SEARCH SINCE` returns those, so the snapshot starts at UID 5 — older mail is
    // never fetched — and reports the in-window count (4), not the mailbox's 8.
    let select = select_resp("a2", 1000, 9, 8);
    let search = "* SEARCH 5 6 7 8\r\na3 OK SEARCH done\r\n";
    let fetch = fetch_resp("a4", &[5, 6, 7, 8]);
    let server = script(&[GREETING, LOGIN_OK, &select, search, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 0, Some("1-Mar-2026"))
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.total, Some(4));
    assert_eq!(page.changed.len(), 4);
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u8@INBOX");
    assert_eq!(page.changed[3].id.as_str(), "imap:v1000:u5@INBOX");
    assert_eq!(page.present.len(), 4);
    let sent = written(&recorded);
    assert!(sent.contains("UID SEARCH SINCE 1-Mar-2026"), "{sent}");
    // The fetch starts at the window floor (5), not UID 1.
    assert!(sent.contains("UID FETCH 5:8"), "{sent}");
}

#[tokio::test]
async fn a_windowed_snapshot_with_no_matches_fetches_nothing() {
    // Nothing in the window → an empty `UID SEARCH` → no `FETCH` at all, but an empty
    // present set still tombstones any stale local rows below the window.
    let select = select_resp("a2", 1000, 9, 8);
    let search = "* SEARCH\r\na3 OK SEARCH done\r\n";
    let server = script(&[GREETING, LOGIN_OK, &select, search]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 0, Some("1-Jun-2026"))
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert!(page.changed.is_empty());
    assert!(page.present.is_empty());
    assert_eq!(page.total, Some(0));
    assert_eq!(page.next_cursor.as_str(), "v1000;n9");
    assert!(!written(&recorded).contains("UID FETCH"));
}

#[tokio::test]
async fn a_delta_ignores_the_sync_depth_window() {
    // A delta is already bounded to new arrivals, so the window triggers no `SEARCH` —
    // new mail is recent by definition.
    let select = select_resp("a2", 1000, 8, 7);
    let fetch = fetch_resp("a3", &[5, 6, 7]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let cursor = SyncState::new("v1000;n5");
    let page = sync_page(
        &mut conn,
        &inbox(),
        Some(&cursor),
        None,
        50,
        Some("1-Mar-2026"),
    )
    .await
    .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert_eq!(page.changed.len(), 3);
    let sent = written(&recorded);
    assert!(
        !sent.contains("UID SEARCH"),
        "a delta must not SEARCH: {sent}"
    );
    assert!(sent.contains("UID FETCH 5:7"));
}

#[tokio::test]
async fn a_windowed_snapshot_fetches_only_the_in_window_uids_not_the_range() {
    // The in-window UIDs are *scattered* (2, then 50, 51) across a large mailbox — moved or
    // imported mail puts a recent message at a low UID. Fetching the range 2:51 would pull
    // ~50 old messages; the windowed snapshot must fetch ONLY {2, 50, 51}, so `fetched`
    // (3) equals the in-window `total` (3) and the download is bounded to the window.
    let select = select_resp("a2", 1000, 100, 60);
    let search = "* SEARCH 2 50 51\r\na3 OK SEARCH done\r\n";
    let fetch = fetch_resp("a4", &[2, 50, 51]);
    let server = script(&[GREETING, LOGIN_OK, &select, search, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 0, Some("1-Mar-2026"))
        .await
        .unwrap();
    assert_eq!(page.total, Some(3));
    assert_eq!(page.changed.len(), 3); // not the ~50 of the 2:51 range
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u51@INBOX"); // newest first
    assert_eq!(page.changed[2].id.as_str(), "imap:v1000:u2@INBOX");
    let sent = written(&recorded);
    // The exact compacted set — never the spanning range or "from UID 1".
    assert!(sent.contains("UID FETCH 2,50:51"), "{sent}");
    assert!(
        !sent.contains("2:51"),
        "must not fetch the spanning range: {sent}"
    );
}

#[tokio::test]
async fn a_windowed_snapshot_pages_the_in_window_set_newest_first() {
    // 4 in-window UIDs (10,20,30,40), limit 2: page one fetches the newest two as an exact
    // set, and hands back a boundary the next page resumes below.
    let select = select_resp("a2", 1000, 50, 40);
    let search = "* SEARCH 10 20 30 40\r\na3 OK SEARCH done\r\n";
    let fetch = fetch_resp("a4", &[30, 40]);
    let server = script(&[GREETING, LOGIN_OK, &select, search, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = sync_page(&mut conn, &inbox(), None, None, 2, Some("1-Mar-2026"))
        .await
        .unwrap();
    assert_eq!(page.total, Some(4)); // the full in-window count, across pages
    assert_eq!(page.changed.len(), 2);
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u40@INBOX");
    assert_eq!(page.changed[1].id.as_str(), "imap:v1000:u30@INBOX");
    // The next page resumes below the lowest kept UID (30).
    assert_eq!(page_high(page.next_page.as_ref().unwrap()), Some(30));
    // 30 and 40 aren't contiguous, so the set is the comma form, not a range.
    assert!(written(&recorded).contains("UID FETCH 30,40"));
}

/// A `SELECT (CONDSTORE)` response, advertising a `HIGHESTMODSEQ` alongside the UID
/// space — what a QRESYNC session opens the mailbox with.
fn select_condstore_resp(
    tag: &str,
    validity: u32,
    uid_next: u32,
    exists: u32,
    modseq: u64,
) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n\
         * OK [UIDNEXT {uid_next}] n\r\n* OK [HIGHESTMODSEQ {modseq}] m\r\n\
         {tag} OK [READ-WRITE] done\r\n"
    )
}

#[tokio::test]
async fn a_qresync_delta_reconciles_flag_changes_and_expunges() {
    // Prior cursor carries a modseq baseline (9); the QRESYNC session opens CONDSTORE
    // (modseq now 20) and one `CHANGEDSINCE 9 VANISHED` brings back the flag-changed
    // UID 6 and the expunged UID 3.
    let select = select_condstore_resp("a2", 1000, 8, 7, 20);
    let delta = format!("* VANISHED (EARLIER) 3\r\n{}", fetch_resp("a3", &[6]));
    let server = script(&[GREETING, LOGIN_OK, &select, &delta]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.force_qresync();

    let cursor = SyncState::new("v1000;n5;m9");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50, None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u6@INBOX");
    assert_eq!(page.removed.len(), 1);
    assert_eq!(page.removed[0].as_str(), "imap:v1000:u3@INBOX");
    // The new modseq baseline rides the cursor forward.
    assert_eq!(page.next_cursor.as_str(), "v1000;n8;m20");

    let sent = written(&recorded);
    assert!(sent.contains("SELECT \"INBOX\" (CONDSTORE)"), "{sent}");
    assert!(sent.contains("(CHANGEDSINCE 9 VANISHED)"), "{sent}");
}

#[tokio::test]
async fn a_qresync_snapshot_records_the_modseq_baseline() {
    // A first sync on a QRESYNC session opens CONDSTORE and records HIGHESTMODSEQ, so
    // the *next* sync can run an incremental delta.
    let select = select_condstore_resp("a2", 1000, 4, 3, 12);
    let fetch = fetch_resp("a3", &[1, 2, 3]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.force_qresync();

    let page = sync_page(&mut conn, &inbox(), None, None, 50, None)
        .await
        .unwrap();
    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 3);
    assert_eq!(page.next_cursor.as_str(), "v1000;n4;m12");
    assert!(written(&recorded).contains("SELECT \"INBOX\" (CONDSTORE)"));
}

#[tokio::test]
async fn the_first_sync_after_upgrade_re_snapshots_to_establish_the_baseline() {
    // A QRESYNC session inherits a pre-QRESYNC cursor (no `;m`): there is no baseline
    // to run `CHANGEDSINCE` from, and a plain new-arrivals delta would silently skip
    // flag/expunge changes to already-synced mail (then record a modseq that hides the
    // gap forever). So this one pass re-snapshots — reconciling the whole mailbox and
    // recording the fresh modseq — and the *next* sync is an incremental delta.
    let select = select_condstore_resp("a2", 1000, 8, 7, 20);
    let fetch = fetch_resp("a3", &[1, 2, 3, 4, 5, 6, 7]);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.force_qresync();

    let cursor = SyncState::new("v1000;n5"); // pre-QRESYNC: no modseq
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 50, None)
        .await
        .unwrap();
    assert_eq!(
        page.kind,
        SyncKind::Snapshot,
        "re-snapshots to reconcile + establish the modseq baseline"
    );
    assert_eq!(
        page.changed.len(),
        7,
        "the whole mailbox, not just new arrivals"
    );
    assert_eq!(
        page.present.len(),
        7,
        "a snapshot tombstones against the full set"
    );
    assert_eq!(page.next_cursor.as_str(), "v1000;n8;m20");
    let sent = written(&recorded);
    assert!(!sent.contains("CHANGEDSINCE"), "no baseline yet: {sent}");
    assert!(
        sent.contains("UID FETCH 1:7"),
        "snapshots from UID 1: {sent}"
    );
}
