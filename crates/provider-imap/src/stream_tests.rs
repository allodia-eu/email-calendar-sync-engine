//! Offline tests for the resumable, incremental streaming backfill, over a mock
//! stream: chunk boundaries, per-group `backfill_low` checkpoints, resume-below-the-
//! watermark, the sync-depth window, and delegation to the page path for a delta.

use engine_core::{ids::MailboxId, sync::SyncWindow};
use engine_provider::{EmailChunk, PassMode};
use futures_util::StreamExt;
use tokio::sync::Mutex;

use super::stream_email;
use crate::{
    cursor::MailboxCursor,
    mock::{MockStream, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

fn select_resp(tag: &str, validity: u32, uid_next: u32, exists: u32) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n\
         * OK [UIDNEXT {uid_next}] n\r\n{tag} OK [READ-WRITE] done\r\n"
    )
}

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

fn search_resp(tag: &str, uids: &[u32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::from("* SEARCH");
    for uid in uids {
        write!(out, " {uid}").unwrap();
    }
    out.push_str("\r\n");
    write!(out, "{tag} OK SEARCH done\r\n").unwrap();
    out
}

fn inbox() -> MailboxId {
    MailboxId::try_from("INBOX").unwrap()
}

async fn logged_in(server: Vec<u8>) -> (Mutex<Connection<MockStream>>, crate::mock::Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (Mutex::new(conn), recorded)
}

/// Drains a streamed pass into its chunks.
async fn drain(
    conn: &Mutex<Connection<MockStream>>,
    cursor: Option<&str>,
    batch: usize,
    chunk: usize,
) -> Vec<EmailChunk> {
    let state = cursor.map(engine_core::sync::SyncState::new);
    let mailbox = inbox();
    let mut stream = Box::pin(stream_email(
        conn,
        &mailbox,
        state.as_ref(),
        SyncWindow::full(),
        batch,
        chunk,
    ));
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    chunks
}

fn key_of(chunk: &EmailChunk, i: usize) -> String {
    chunk.changed[i].id.key().as_str().to_owned()
}

#[tokio::test]
async fn cold_backfill_streams_newest_group_first_and_checkpoints_each_group() {
    // UIDs 1..=8 (UIDNEXT 9). fetch_batch 3 → descending groups 6:8, 3:5, 1:2.
    let select = select_resp("a2", 1000, 9, 8);
    let server = script(&[
        GREETING,
        LOGIN_OK,
        &select,
        &fetch_resp("a3", &[6, 7, 8]),
        &fetch_resp("a4", &[3, 4, 5]),
        &fetch_resp("a5", &[1, 2]),
    ]);
    let (conn, recorded) = logged_in(server).await;

    // chunk_size 0 → one chunk per group (3 groups); the last carries the completed
    // cursor, so no extra marker.
    let chunks = drain(&conn, None, 3, 0).await;

    assert_eq!(chunks.len(), 3);
    // The intermediate groups are additive checkpoints; the last reconciles.
    assert_eq!(chunks[0].mode, PassMode::Additive);
    assert_eq!(chunks[1].mode, PassMode::Additive);
    // Newest group committed first, so the newest UID is in the first chunk.
    assert_eq!(key_of(&chunks[0], 0), "imap:v1000:u6@INBOX");
    assert_eq!(chunks[0].changed.len(), 3);
    // Each intermediate group checkpoints its lowest UID, so a kill resumes below it.
    assert_eq!(
        chunks[0].advance_to.as_ref().unwrap().as_str(),
        "v1000;n9;b6"
    );
    assert_eq!(
        chunks[1].advance_to.as_ref().unwrap().as_str(),
        "v1000;n9;b3"
    );
    // The last group reconciles against the full present set and clears the watermark.
    assert!(chunks[2].is_reconcile_final());
    assert_eq!(
        chunks[2].present.len(),
        8,
        "all eight UIDs drive tombstoning"
    );
    assert_eq!(chunks[2].advance_to.as_ref().unwrap().as_str(), "v1000;n9");
    // The newest window was fetched first.
    let sent = written(&recorded);
    assert!(sent.contains("UID FETCH 6:8"));
    assert!(sent.contains("UID FETCH 1:2"));
}

#[tokio::test]
async fn a_small_chunk_size_commits_within_a_group() {
    // One group (6:8) but chunk_size 2 → two chunks: an intermediate held pair, then
    // the group's terminal checkpointed chunk — row-as-it-arrives within one FETCH.
    let select = select_resp("a2", 1000, 9, 8);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch_resp("a3", &[6, 7, 8])]);
    let (conn, _) = logged_in(server).await;

    let chunks = drain(&conn, None, 8, 2).await;
    // Held pair (no checkpoint), then the group's terminal chunk. It is the last (and
    // only) group of a fresh pass, so it reconciles and advances to the completed cursor.
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].changed.len(), 2);
    assert!(
        chunks[0].advance_to.is_none(),
        "an intermediate chunk holds the cursor"
    );
    assert_eq!(chunks[1].changed.len(), 1);
    assert!(chunks[1].is_reconcile_final());
    assert_eq!(
        chunks[1].present.len(),
        3,
        "the whole group's UIDs drive tombstoning"
    );
    assert_eq!(chunks[1].advance_to.as_ref().unwrap().as_str(), "v1000;n9");
}

#[tokio::test]
async fn a_backfill_resumes_below_its_watermark() {
    // A prior cursor at watermark 6 means UIDs 6..=8 are already synced; the resume
    // fetches only below it (high = 5), newest-first: groups 3:5, 1:2.
    let select = select_resp("a2", 1000, 9, 8);
    let server = script(&[
        GREETING,
        LOGIN_OK,
        &select,
        &fetch_resp("a3", &[3, 4, 5]),
        &fetch_resp("a4", &[1, 2]),
    ]);
    let (conn, recorded) = logged_in(server).await;

    let chunks = drain(&conn, Some("v1000;n9;b6"), 3, 0).await;
    // Two group chunks (last completes); the already-synced group (6:8) is NOT refetched.
    assert_eq!(chunks.len(), 2);
    assert_eq!(key_of(&chunks[0], 0), "imap:v1000:u3@INBOX");
    assert_eq!(
        chunks[0].advance_to.as_ref().unwrap().as_str(),
        "v1000;n9;b3"
    );
    assert_eq!(chunks[1].advance_to.as_ref().unwrap().as_str(), "v1000;n9");
    // A resume saw only part of the set this session, so it completes additively (no
    // tombstone) rather than reconciling against a partial present set.
    assert!(chunks.iter().all(|c| !c.is_reconcile_final()));
    let sent = written(&recorded);
    assert!(
        !sent.contains("UID FETCH 6:8"),
        "already-synced group not refetched"
    );
    assert!(sent.contains("UID FETCH 3:5"));
}

#[tokio::test]
async fn a_windowed_backfill_fetches_only_the_in_window_uids() {
    // A sync-depth window: `UID SEARCH SINCE` reports UIDs 5,7,8 in window; the backfill
    // fetches exactly those (newest-first), never the whole 1..=8 range.
    let select = select_resp("a2", 1000, 9, 8);
    let server = script(&[
        GREETING,
        LOGIN_OK,
        &select,
        &search_resp("a3", &[5, 7, 8]),
        &fetch_resp("a4", &[7, 8]), // group 7,8 (fetch_batch 2, newest first)
        &fetch_resp("a5", &[5]),
    ]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let conn = Mutex::new(conn);

    let window = SyncWindow::since(engine_core::time::CalendarDate::new(2026, 1, 1).unwrap());
    let mailbox = inbox();
    let mut stream = Box::pin(stream_email(&conn, &mailbox, None, window, 2, 0));
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.unwrap());
    }
    let upserted: usize = chunks.iter().map(|c| c.changed.len()).sum();
    assert_eq!(upserted, 3, "exactly the three in-window messages");
    let sent = written(&recorded);
    assert!(sent.contains("UID SEARCH SINCE 1-Jan-2026"));
    assert!(
        !sent.contains("UID FETCH 1:"),
        "out-of-window UIDs never fetched"
    );
}

#[tokio::test]
async fn a_delta_delegates_to_the_page_path_and_is_additive() {
    // A prior complete cursor (no watermark) matching validity → a new-arrivals delta:
    // UIDNEXT advanced 9 → 11, so UIDs 9,10 are fetched as an additive pass.
    let select = select_resp("a2", 1000, 11, 10);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch_resp("a3", &[9, 10])]);
    let (conn, _) = logged_in(server).await;

    let chunks = drain(&conn, Some("v1000;n9"), 50, 0).await;
    let upserted: usize = chunks.iter().map(|c| c.changed.len()).sum();
    assert_eq!(upserted, 2, "the two new arrivals");
    assert!(chunks.iter().all(|c| c.mode == PassMode::Additive));
    assert!(!chunks.last().unwrap().is_reconcile_final());
    // Advances to the fresh complete cursor.
    assert_eq!(
        chunks.last().unwrap().advance_to.as_ref().unwrap().as_str(),
        "v1000;n11"
    );
}

#[tokio::test]
async fn a_uidvalidity_reset_reconciles_via_the_page_path() {
    // The prior cursor's validity (999) no longer matches (1000) → a reset: every UID
    // is rediscovered as a reconciling snapshot that tombstones the renumbered rows.
    let select = select_resp("a2", 1000, 4, 3);
    let server = script(&[GREETING, LOGIN_OK, &select, &fetch_resp("a3", &[1, 2, 3])]);
    let (conn, _) = logged_in(server).await;

    let chunks = drain(&conn, Some("v999;n50"), 50, 0).await;
    assert!(chunks.iter().any(|c| c.mode == PassMode::Reconcile));
    assert!(chunks.last().unwrap().is_reconcile_final());
    let present: usize = chunks.iter().map(|c| c.present.len()).sum();
    assert_eq!(present, 3, "all rediscovered UIDs drive tombstoning");
}

#[test]
fn resume_cursor_roundtrip_is_stable() {
    // The watermark a chunk emits decodes back to the same resume point.
    let cursor = MailboxCursor::decode(&engine_core::sync::SyncState::new("v1000;n9;b6")).unwrap();
    assert_eq!(cursor.backfill_low, Some(6));
    assert_eq!(cursor.uid_next, 9);
}

#[tokio::test]
async fn an_empty_mailbox_backfill_yields_only_a_completing_marker() {
    // No messages (UIDNEXT 1, EXISTS 0): the backfill has no group to fetch, so it
    // emits a single empty completing chunk that advances straight to steady state.
    let select = select_resp("a2", 1000, 1, 0);
    let server = script(&[GREETING, LOGIN_OK, &select]);
    let (conn, recorded) = logged_in(server).await;

    let chunks = drain(&conn, None, 50, 0).await;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].changed.is_empty());
    // A fresh pass reconciles even when empty, so an emptied mailbox tombstones every
    // stale local row against the empty present set.
    assert!(chunks[0].is_reconcile_final());
    assert_eq!(chunks[0].advance_to.as_ref().unwrap().as_str(), "v1000;n1");
    assert!(
        !written(&recorded).contains("UID FETCH"),
        "nothing to fetch"
    );
}

/// Opens a bare (un-`Mutex`ed) connection for the transport-level streamed-fetch tests.
async fn open_conn(server: Vec<u8>) -> Connection<MockStream> {
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn
}

const LIST_OK: &str = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\na3 OK LIST done\r\n";

#[tokio::test]
async fn an_abandoned_streamed_fetch_self_heals_on_the_next_command() {
    // Start a streamed fetch, read only the first row, then issue another command
    // without draining: the connection must finish the leftover response itself so the
    // next command is not corrupted.
    let server = script(&[GREETING, LOGIN_OK, &fetch_resp("a2", &[5, 6, 7]), LIST_OK]);
    let mut conn = open_conn(server).await;

    conn.uid_fetch_stream_start("5:7", "(FLAGS)").await.unwrap();
    let first = conn.next_fetch_row().await.unwrap().expect("a first row");
    assert_eq!(first.uid, 5);

    // A fresh command drains the abandoned fetch (rows 6,7 + its tag) then runs.
    let folders = conn.list().await.unwrap();
    assert_eq!(
        folders.len(),
        1,
        "LIST ran cleanly after the abandoned fetch"
    );
}

#[tokio::test]
async fn a_streamed_fetch_surfaces_a_no_completion_as_an_error() {
    // The command completes `NO`: the second pull returns a classified error.
    let fetch = "* 1 FETCH (UID 5 FLAGS (\\Seen) INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" \
                 RFC822.SIZE 10 ENVELOPE (NIL \"s\" NIL NIL NIL NIL NIL NIL NIL \"<m@h>\") \
                 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 2 1) \
                 BODY[HEADER.FIELDS (REFERENCES)] \"\")\r\na2 NO fetch failed\r\n";
    let server = script(&[GREETING, LOGIN_OK, fetch]);
    let mut conn = open_conn(server).await;

    conn.uid_fetch_stream_start("5:5", "(FLAGS)").await.unwrap();
    assert!(
        conn.next_fetch_row().await.unwrap().is_some(),
        "the row parses"
    );
    assert!(
        conn.next_fetch_row().await.is_err(),
        "the NO completion is an error"
    );
}

#[tokio::test]
async fn a_streamed_fetch_skips_non_fetch_untagged_responses() {
    // A `* n EXISTS` interleaved with the FETCH rows is skipped, not mis-parsed.
    let fetch = "* 9 EXISTS\r\n\
                 * 1 FETCH (UID 5 FLAGS (\\Seen) INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" \
                 RFC822.SIZE 10 ENVELOPE (NIL \"s\" NIL NIL NIL NIL NIL NIL NIL \"<m@h>\") \
                 BODYSTRUCTURE (\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 2 1) \
                 BODY[HEADER.FIELDS (REFERENCES)] \"\")\r\na2 OK done\r\n";
    let server = script(&[GREETING, LOGIN_OK, fetch]);
    let mut conn = open_conn(server).await;

    conn.uid_fetch_stream_start("5:5", "(FLAGS)").await.unwrap();
    let row = conn
        .next_fetch_row()
        .await
        .unwrap()
        .expect("the FETCH row, EXISTS skipped");
    assert_eq!(row.uid, 5);
    assert!(
        conn.next_fetch_row().await.unwrap().is_none(),
        "then the tagged completion"
    );
}
