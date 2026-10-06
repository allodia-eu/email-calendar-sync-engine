//! Offline tests for spreading a structure fetch over spare connections: who is borrowed,
//! which share each connection asks for, and what happens to a share whose helper failed.

use core::fmt::Write as _;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::*;
use crate::{
    capability::Negotiated,
    mock::{MockStream, Recorded, script, written},
    parse::parse_fetch,
    pool::{Dial, ImapPool},
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";
const NEVER_CHECK: Duration = Duration::from_hours(1);
const MIXED: &str = "Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n";
const WITH_PDF: &str = r#"(("TEXT" "PLAIN" NIL NIL NIL "7BIT" 2 1)("APPLICATION" "PDF" ("NAME" "a.pdf") NIL NIL "BASE64" 12 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL) "MIXED")"#;

/// A pool, the count of its dials, and what each dialled connection was sent.
type RecordingPool = (
    Arc<ImapPool<MockStream>>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Recorded>>>,
);

/// A pool whose dials serve `scripts` in order, recording what each connection was sent.
fn pool_of(budget: usize, scripts: Vec<String>) -> RecordingPool {
    let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
    let dials = Arc::new(AtomicUsize::new(0));
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let (counter, recordings) = (Arc::clone(&dials), Arc::clone(&sessions));
    let dial: Dial<MockStream> = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        let next = scripts.lock().unwrap().pop_front().unwrap_or_default();
        let (stream, recorded) = MockStream::new(script(&[GREETING, &next]));
        recordings.lock().unwrap().push(recorded);
        Box::pin(async move { Connection::open(stream).await })
    });
    (ImapPool::new(dial, budget, NEVER_CHECK), dials, sessions)
}

/// Rows the headers left open, for UIDs `uids`.
fn open_rows(uids: &[u32]) -> Vec<FetchRow> {
    let lines: Vec<Vec<u8>> = uids
        .iter()
        .map(|uid| {
            format!(
                "{uid} FETCH (UID {uid} ENVELOPE (NIL \"s\" NIL NIL NIL NIL NIL NIL NIL \"<m{uid}@h>\") \
                 BODY[HEADER.FIELDS (CONTENT-TYPE)] {{{}}}\r\n{MIXED})",
                MIXED.len()
            )
            .into_bytes()
        })
        .collect();
    parse_fetch(&lines).unwrap()
}

/// A `tag`ged answer settling `uids`, each with a downloadable part.
fn structures(tag: &str, uids: &[u32]) -> String {
    let mut out = String::new();
    for uid in uids {
        write!(
            out,
            "* {uid} FETCH (UID {uid} BODYSTRUCTURE {WITH_PDF})\r\n"
        )
        .unwrap();
    }
    write!(out, "{tag} OK FETCH done\r\n").unwrap();
    out
}

async fn own_connection(server: &[&str]) -> (Connection<MockStream>, Recorded) {
    let (stream, recorded) = MockStream::new(script(server));
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

fn examined(tag: &str, validity: u32) -> String {
    format!("* 6 EXISTS\r\n* OK [UIDVALIDITY {validity}] v\r\n{tag} OK [READ-ONLY] done\r\n")
}

#[tokio::test]
async fn each_connection_asks_for_its_own_share_and_the_answers_are_merged() {
    let (pool, _, sessions) = pool_of(
        5,
        vec![structures("a1", &[3, 4]), structures("a1", &[5, 6])],
    );
    let helpers = vec![pool.acquire().await.unwrap(), pool.acquire().await.unwrap()];
    let own = structures("a2", &[1, 2]);
    let (mut conn, recorded) = own_connection(&[GREETING, LOGIN_OK, &own]).await;
    let mut open = open_rows(&[1, 2, 3, 4, 5, 6]);

    let settled = settle_across(&mut conn, helpers, &mut open).await.unwrap();

    assert!(open.is_empty());
    let mut uids: Vec<_> = settled
        .iter()
        .map(|row| (row.uid, row.has_attachment))
        .collect();
    uids.sort_unstable();
    assert_eq!(
        uids,
        (1..=6).map(|uid| (uid, Some(true))).collect::<Vec<_>>()
    );
    assert!(written(&recorded).contains("a2 UID FETCH 1:2 (UID BODYSTRUCTURE)"));
    let sessions = sessions.lock().unwrap();
    assert!(written(&sessions[0]).contains("a1 UID FETCH 3:4 (UID BODYSTRUCTURE)"));
    assert!(written(&sessions[1]).contains("a1 UID FETCH 5:6 (UID BODYSTRUCTURE)"));
}

#[tokio::test]
async fn a_failed_helper_is_discarded_and_its_share_asked_again() {
    // The second helper's server goes away mid-command.
    let (pool, dials, _) = pool_of(5, vec![structures("a1", &[3, 4]), String::new()]);
    let helpers = vec![pool.acquire().await.unwrap(), pool.acquire().await.unwrap()];
    let own = structures("a2", &[1, 2]);
    let retry = structures("a3", &[5, 6]);
    let (mut conn, recorded) = own_connection(&[GREETING, LOGIN_OK, &own, &retry]).await;
    let mut open = open_rows(&[1, 2, 3, 4, 5, 6]);

    let settled = settle_across(&mut conn, helpers, &mut open).await.unwrap();

    assert_eq!(settled.len(), 6);
    assert!(written(&recorded).contains("a3 UID FETCH 5:6 (UID BODYSTRUCTURE)"));
    // The healthy helper was parked and is reused; the failed one was not.
    let _reused = pool.acquire().await.unwrap();
    let _fresh = pool.acquire().await.unwrap();
    assert_eq!(dials.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn helpers_are_borrowed_by_the_size_of_the_work_and_the_budget_left() {
    assert!(
        spare_helpers::<MockStream>(None, "INBOX", 7, 500)
            .await
            .is_empty()
    );

    let scripts = (0..5).map(|_| examined("a1", 7)).collect();
    let (pool, dials, _) = pool_of(5, scripts);
    // Too little work for a second connection.
    assert!(spare_helpers(Some(&pool), "INBOX", 7, 39).await.is_empty());
    assert_eq!(dials.load(Ordering::SeqCst), 0);
    // Never more than three, each with the folder open.
    let _pass = pool.acquire().await.unwrap();
    let helpers = spare_helpers(Some(&pool), "INBOX", 7, 500).await;
    assert_eq!(helpers.len(), 3);
    assert!(
        helpers
            .iter()
            .all(|helper| helper.open_validity("INBOX") == Some(7))
    );
}

#[tokio::test]
async fn a_spent_budget_or_a_moved_folder_lends_no_helper() {
    let (pool, ..) = pool_of(2, vec![String::new(), examined("a1", 8)]);
    let _pass = pool.acquire().await.unwrap();
    // The one spare connection finds the folder at another UIDVALIDITY.
    assert!(spare_helpers(Some(&pool), "INBOX", 7, 500).await.is_empty());

    let _other = pool.acquire().await.unwrap();
    assert!(spare_helpers(Some(&pool), "INBOX", 7, 500).await.is_empty());
}

#[tokio::test]
async fn every_share_keeps_to_the_servers_message_limit() {
    let helper = format!("{}{}", structures("a1", &[4, 5]), structures("a2", &[6]));
    let (pool, _, sessions) = pool_of(5, vec![helper]);
    let helpers = vec![pool.acquire().await.unwrap()];
    let (own_first, own_second) = (structures("a2", &[1, 2]), structures("a3", &[3]));
    let (mut conn, recorded) = own_connection(&[GREETING, LOGIN_OK, &own_first, &own_second]).await;
    conn.negotiated = Negotiated::from_capabilities(&["MESSAGELIMIT=2".to_owned()]);
    let mut open = open_rows(&[1, 2, 3, 4, 5, 6]);

    let settled = settle_across(&mut conn, helpers, &mut open).await.unwrap();

    assert_eq!(settled.len(), 6);
    let own = written(&recorded);
    assert!(
        own.contains("a2 UID FETCH 1:2 (UID BODYSTRUCTURE)"),
        "{own}"
    );
    assert!(own.contains("a3 UID FETCH 3 (UID BODYSTRUCTURE)"), "{own}");
    let helper = written(&sessions.lock().unwrap()[0]);
    assert!(
        helper.contains("a1 UID FETCH 4:5 (UID BODYSTRUCTURE)"),
        "{helper}"
    );
    assert!(
        helper.contains("a2 UID FETCH 6 (UID BODYSTRUCTURE)"),
        "{helper}"
    );
}
