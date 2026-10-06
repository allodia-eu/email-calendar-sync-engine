//! Offline tests for the delta of a session without QRESYNC: the flags of the mail already
//! synced and the set still on the server, beside the arrivals. The flag rows are shaped like
//! Yahoo's (`FLAGS` before `UID`, `$NotJunk` on everything), a server that offers no QRESYNC.

use engine_core::mail::{Keyword, SystemKeyword};
use engine_provider::SyncKind;

use super::*;
use crate::mock::{MockStream, Recorded, script, written};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

async fn logged_in(server: Vec<u8>) -> (Connection<MockStream>, Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

fn inbox() -> MailboxId {
    MailboxId::try_from("INBOX").unwrap()
}

fn select_resp(validity: u32, uid_next: u32, exists: u32) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n\
         * OK [UIDNEXT {uid_next}] n\r\na2 OK [READ-WRITE] done\r\n"
    )
}

/// `(UID FLAGS)` rows, one per `(uid, flags)`.
fn flags_resp(tag: &str, rows: &[(u32, &str)]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for (index, (uid, flags)) in rows.iter().enumerate() {
        write!(out, "* {} FETCH (FLAGS ({flags}) UID {uid})\r\n", index + 1).unwrap();
    }
    write!(out, "{tag} OK FETCH completed\r\n").unwrap();
    out
}

/// Whole rows for the arrivals, as the snapshot path fetches them.
fn full_resp(tag: &str, uids: &[u32]) -> String {
    use core::fmt::Write as _;
    let mut out = String::new();
    for uid in uids {
        write!(
            out,
            "* {uid} FETCH (UID {uid} FLAGS () INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" \
             RFC822.SIZE 10 ENVELOPE (NIL \"s{uid}\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\") \
             BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 2 1) \
             BODY[HEADER.FIELDS (REFERENCES)] \"\")\r\n"
        )
        .unwrap();
    }
    write!(out, "{tag} OK FETCH completed\r\n").unwrap();
    out
}

fn key(uid: u32) -> String {
    format!("imap:v1000:u{uid}@INBOX")
}

#[tokio::test]
async fn without_new_mail_the_delta_reconciles_the_flags_of_what_is_stored() {
    // Nothing arrived since UIDNEXT 10; UID 5 was flagged elsewhere and UIDs 1, 3, 4, 6, 7
    // and 8 are gone. The page is the whole truth below the watermark, so it reconciles.
    let select = select_resp(1000, 10, 3);
    let flags = flags_resp(
        "a3",
        &[
            (2, "\\Seen $NotJunk"),
            (5, "\\Flagged $NotJunk"),
            (9, "$NotJunk"),
        ],
    );
    let (mut conn, recorded) = logged_in(script(&[GREETING, LOGIN_OK, &select, &flags])).await;

    let cursor = SyncState::new("v1000;n10;g1");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 0, None)
        .await
        .unwrap();

    assert_eq!(page.kind, SyncKind::Snapshot);
    assert!(page.changed.is_empty());
    assert_eq!(page.present.len(), 3);
    let flagged = page
        .patched
        .iter()
        .find(|change| change.key.as_str() == key(5))
        .unwrap();
    assert!(
        flagged
            .state
            .keywords
            .contains(&Keyword::system(SystemKeyword::Flagged))
    );
    assert_eq!(page.patched.len(), 3);
    assert_eq!(page.next_cursor.as_str(), "v1000;n10;g1;r");
    assert!(page.next_page.is_none());
    assert!(written(&recorded).contains("UID FETCH 1:9 (UID FLAGS)"));
}

#[tokio::test]
async fn new_mail_comes_whole_and_the_mail_already_held_comes_as_its_flags() {
    // UIDNEXT 5 → 8: 5, 6 and 7 arrived. Below 5 only flags are fetched.
    let select = select_resp(1000, 8, 5);
    let flags = flags_resp("a3", &[(1, "\\Seen"), (3, "")]);
    let full = full_resp("a4", &[5, 6, 7]);
    let (mut conn, recorded) =
        logged_in(script(&[GREETING, LOGIN_OK, &select, &flags, &full])).await;

    let cursor = SyncState::new("v1000;n5;g1");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 0, None)
        .await
        .unwrap();

    assert_eq!(page.kind, SyncKind::Snapshot);
    let changed: Vec<&str> = page.changed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(changed, [key(7), key(6), key(5)]);
    assert_eq!(page.patched.len(), 2);
    assert_eq!(page.present.len(), 5);
    assert_eq!(page.total, Some(3));
    assert_eq!(page.next_cursor.as_str(), "v1000;n8;g1;r");
    let sent = written(&recorded);
    assert!(sent.contains("UID FETCH 1:4 (UID FLAGS)"), "{sent}");
    assert!(
        sent.contains("UID FETCH 5:7 (UID FLAGS INTERNALDATE"),
        "{sent}"
    );
}

#[tokio::test]
async fn the_sync_depth_window_bounds_both_halves() {
    // Only 3, 4, 6 and 7 are in the window: older mail is neither fetched nor present.
    let select = select_resp(1000, 8, 7);
    let search = "* SEARCH 3 4 6 7\r\na3 OK SEARCH completed\r\n";
    let flags = flags_resp("a4", &[(3, "\\Seen"), (4, "")]);
    let full = full_resp("a5", &[6, 7]);
    let (mut conn, recorded) = logged_in(script(&[
        GREETING, LOGIN_OK, &select, search, &flags, &full,
    ]))
    .await;

    let cursor = SyncState::new("v1000;n5;g1");
    let page = sync_page(
        &mut conn,
        &inbox(),
        Some(&cursor),
        None,
        0,
        Some("1-Mar-2026"),
    )
    .await
    .unwrap();

    assert_eq!(page.kind, SyncKind::Snapshot);
    assert_eq!(page.changed.len(), 2);
    assert_eq!(page.patched.len(), 2);
    assert_eq!(page.present.len(), 4);
    let sent = written(&recorded);
    assert!(sent.contains("UID SEARCH SINCE 1-Mar-2026"), "{sent}");
    assert!(sent.contains("UID FETCH 3:4 (UID FLAGS)"), "{sent}");
    assert!(
        sent.contains("UID FETCH 6:7 (UID FLAGS INTERNALDATE"),
        "{sent}"
    );
}

#[tokio::test]
async fn each_fetch_names_no_more_messages_than_one_batch() {
    // A server may cap how many messages one command touches (RFC 9738 `MESSAGELIMIT`;
    // Yahoo's is 1000), so both halves go out in batches of `limit` UIDs.
    let select = select_resp(1000, 12, 10);
    let first = flags_resp("a3", &[(1, ""), (2, "")]);
    let second = flags_resp("a4", &[(5, "")]);
    let third = flags_resp("a5", &[(9, "")]);
    let full = full_resp("a6", &[10, 11]);
    let (mut conn, recorded) = logged_in(script(&[
        GREETING, LOGIN_OK, &select, &first, &second, &third, &full,
    ]))
    .await;

    let cursor = SyncState::new("v1000;n10;g1");
    let page = sync_page(&mut conn, &inbox(), Some(&cursor), None, 4, None)
        .await
        .unwrap();

    assert_eq!(page.present.len(), 6);
    let sent = written(&recorded);
    for command in [
        "UID FETCH 1:4 (UID FLAGS)",
        "UID FETCH 5:8 (UID FLAGS)",
        "UID FETCH 9:9 (UID FLAGS)",
        "UID FETCH 10:11 (UID FLAGS INTERNALDATE",
    ] {
        assert!(sent.contains(command), "{command} in {sent}");
    }
}
