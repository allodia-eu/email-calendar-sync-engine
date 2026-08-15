//! Offline tests for the QRESYNC incremental delta, replaying the **exact** bytes
//! captured from a live Stalwart session (an observed provider transcript, per
//! `providers.md`).

use engine_core::mail::{Keyword, SystemKeyword};
use engine_provider::SyncKind;

use super::*;
use crate::mock::{MockStream, script, written};

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

fn system(keyword: SystemKeyword) -> Keyword {
    Keyword::system(keyword)
}

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// Stalwart's real answer to the state half — `UID FETCH 1:68 (UID FLAGS)
/// (CHANGEDSINCE 225 VANISHED)` after UID 56 was flagged and UID 68 expunged.
///
/// The `MODSEQ (227)` is the server's, not ours: with CONDSTORE enabled it must attach
/// one to every `FETCH` a `CHANGEDSINCE` caused (RFC 7162 §3.1.4.1). The parser ignores
/// it. Note the absent `ENVELOPE` — that is the point of this half, and what routes the
/// row to `patched`.
const STATE_HALF: &str = "* VANISHED (EARLIER) 68\r\n\
     * 2 FETCH (UID 56 FLAGS (\\Flagged \\Seen) MODSEQ (227))\r\n\
     a2 OK UID FETCH completed\r\n";

#[tokio::test]
async fn a_flag_change_to_synced_mail_costs_flags_alone() {
    let server = script(&[GREETING, LOGIN_OK, STATE_HALF]);
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let next_cursor = SyncState::new("v2021165119;n69;m227");
    // Prior UIDNEXT 69 == this SELECT's, so nothing arrived: the state half is the
    // whole pass.
    let page = delta_page(
        &mut conn,
        &inbox(),
        2_021_165_119,
        next_cursor.clone(),
        225,
        69,
        69,
    )
    .await
    .unwrap();

    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.next_page.is_none());
    assert!(page.present.is_empty(), "a delta carries no present set");
    assert_eq!(page.next_cursor, next_cursor);

    // The flag change is a state change, not a message — nothing here can rewrite the
    // stored subject, sender or preview.
    assert!(
        page.changed.is_empty(),
        "a flag change is not a whole object"
    );
    assert_eq!(page.patched.len(), 1);
    let change = &page.patched[0];
    assert_eq!(change.key.as_str(), "imap:v2021165119:u56@INBOX");
    assert!(change.state.keywords.contains(&system(SystemKeyword::Seen)));
    assert!(
        change
            .state
            .keywords
            .contains(&system(SystemKeyword::Flagged))
    );

    // The expunged UID is a removal — the store tombstones it inline (no snapshot).
    assert_eq!(page.removed.len(), 1);
    assert_eq!(page.removed[0].as_str(), "imap:v2021165119:u68@INBOX");

    // One command, and it asked for flags — not the envelope/bodystructure the old
    // shape pulled for every changed message.
    let sent = written(&recorded);
    assert!(
        sent.contains("UID FETCH 1:68 (UID FLAGS) (CHANGEDSINCE 225 VANISHED)"),
        "{sent}"
    );
    assert!(
        !sent.contains("ENVELOPE"),
        "a flag-only delta must not fetch metadata: {sent}"
    );
}

#[tokio::test]
async fn an_arrival_still_comes_back_whole() {
    // Prior UIDNEXT 10, current 12: UIDs below 10 are the state half (empty here), and
    // 10 upward are new — those need the metadata a first sync stores.
    let state = "a2 OK UID FETCH completed\r\n";
    let arrivals = "* 5 FETCH (UID 11 FLAGS (\\Seen) \
         INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 20 \
         ENVELOPE (NIL \"real subject\" ((\"A\" NIL \"a\" \"h\")) NIL NIL NIL NIL NIL NIL \"<m11@h>\"))\r\n\
         a3 OK UID FETCH completed\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, state, arrivals]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n12;m40"),
        9,
        10,
        12,
    )
    .await
    .unwrap();

    assert!(page.patched.is_empty());
    assert_eq!(page.changed.len(), 1);
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u11@INBOX");
    assert_eq!(
        page.changed[0].envelope.subject.as_deref(),
        Some("real subject")
    );

    let sent = written(&recorded);
    assert!(
        sent.contains("UID FETCH 1:9 (UID FLAGS) (CHANGEDSINCE 9 VANISHED)"),
        "{sent}"
    );
    // The arrivals half asks from the prior UIDNEXT up, with the full item list, and
    // needs no CHANGEDSINCE — a UID that high was assigned after the baseline.
    assert!(
        sent.contains("UID FETCH 10:* (UID FLAGS INTERNALDATE"),
        "{sent}"
    );
}

#[tokio::test]
async fn an_idle_mailbox_does_not_refetch_its_newest_message() {
    // `UID FETCH 10:*` on a mailbox whose highest UID is 9 returns UID 9 (RFC 9051
    // §6.4.8 — `*` is the highest UID, and a range containing it matches). Unguarded,
    // every sync of an idle mailbox would re-fetch its newest message as an arrival and
    // rewrite that message's payload. The guard is `uid_next > synced_below`.
    let state = "a2 OK UID FETCH completed\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, state]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n10;m40"),
        9,
        10,
        10,
    )
    .await
    .unwrap();

    assert!(page.changed.is_empty());
    let sent = written(&recorded);
    assert!(
        !sent.contains("10:*"),
        "no arrivals fetch when UIDNEXT has not moved: {sent}"
    );
}

#[tokio::test]
async fn a_mailbox_with_nothing_synced_yet_asks_only_for_arrivals() {
    // A prior UIDNEXT of 1 means the mailbox was empty at the last sync: there is no
    // already-synced half to reconcile and nothing an expunge could remove from us, so
    // the state command (and its `1:0` range) is never issued.
    let arrivals = "* 1 FETCH (UID 1 FLAGS () \
         INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 20 \
         ENVELOPE (NIL \"first\" ((\"A\" NIL \"a\" \"h\")) NIL NIL NIL NIL NIL NIL \"<m1@h>\"))\r\n\
         a2 OK UID FETCH completed\r\n";
    let (stream, recorded) = MockStream::new(script(&[GREETING, LOGIN_OK, arrivals]));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n2;m4"),
        1,
        1,
        2,
    )
    .await
    .unwrap();

    assert_eq!(page.changed.len(), 1);
    assert!(page.patched.is_empty());
    let sent = written(&recorded);
    assert!(!sent.contains("1:0"), "no empty state range: {sent}");
    assert!(!sent.contains("CHANGEDSINCE"), "{sent}");
}

#[tokio::test]
async fn an_unsolicited_flag_row_becomes_a_state_change() {
    // Once CONDSTORE is on the server may interleave an *unsolicited* flag-only
    // `* n FETCH (UID x FLAGS (..) MODSEQ (..))` — no ENVELOPE — for a message another
    // client changed mid-fetch (RFC 7162 §3.2). Mapping it as a message would upsert an
    // empty envelope over UID 9's good metadata; it is a state change, and now says so.
    let state = "a2 OK UID FETCH completed\r\n";
    let arrivals = "* 2 FETCH (UID 11 FLAGS (\\Flagged \\Seen) \
         INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 20 \
         ENVELOPE (NIL \"real subject\" ((\"A\" NIL \"a\" \"h\")) NIL NIL NIL NIL NIL NIL \"<m2@h>\"))\r\n\
         * 9 FETCH (UID 9 FLAGS (\\Seen) MODSEQ (40))\r\n\
         a3 OK UID FETCH completed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, state, arrivals])).await;

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n12;m40"),
        9,
        10,
        12,
    )
    .await
    .unwrap();

    assert_eq!(page.changed.len(), 1, "only the solicited row is a message");
    assert_eq!(page.changed[0].id.as_str(), "imap:v1000:u11@INBOX");
    assert_eq!(
        page.changed[0].envelope.subject.as_deref(),
        Some("real subject"),
        "the solicited row keeps its real metadata"
    );
    assert_eq!(page.patched.len(), 1, "the notification is not discarded");
    assert_eq!(page.patched[0].key.as_str(), "imap:v1000:u9@INBOX");
    assert!(
        page.patched[0]
            .state
            .keywords
            .contains(&system(SystemKeyword::Seen))
    );
}

#[tokio::test]
async fn a_qresync_delta_with_no_changes_is_empty() {
    // A `CHANGEDSINCE` that matched nothing: no FETCH rows, no VANISHED — a clean,
    // empty delta that still advances the cursor.
    let resp = "a2 OK UID FETCH completed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;

    let page = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n5;m9"),
        9,
        5,
        5,
    )
    .await
    .unwrap();
    assert_eq!(page.kind, SyncKind::Delta);
    assert!(page.changed.is_empty());
    assert!(page.patched.is_empty());
    assert!(page.removed.is_empty());
    assert!(page.next_page.is_none());
}

#[tokio::test]
async fn a_qresync_delta_surfaces_a_fetch_error() {
    // A tagged NO on the CHANGEDSINCE fetch propagates as a classified error (not a
    // silent empty delta), so the orchestrator can reclassify/retry.
    let resp = "a2 NO [SERVERBUG] fetch failed\r\n";
    let mut conn = logged_in(script(&[GREETING, LOGIN_OK, resp])).await;
    let err = delta_page(
        &mut conn,
        &inbox(),
        1000,
        SyncState::new("v1000;n5;m9"),
        9,
        5,
        5,
    )
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
        10,
        10,
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
