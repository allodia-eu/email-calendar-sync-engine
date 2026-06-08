//! Offline end-to-end: drive the IMAP `Provider` through `engine-sync`'s streaming
//! loop into a real `SqliteStore`, all over a mock stream (no Docker, no TLS).
//!
//! This proves the adapter composes with the orchestrator exactly like the JMAP
//! client does: folder container before email members, each page committed and
//! host-visible as it lands, progress reported per page, and the derived FTS rows
//! making the mail searchable — the whole cycle the store contract prescribes.

use core::fmt::Write as _;
use core::time::Duration;
use std::sync::Mutex;

use engine_core::ids::AccountId;
use engine_search::MailQuery;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{SyncProgress, sync_mail_streamed};
use store_sqlite::SqliteStore;

use crate::ImapProvider;
use crate::mock::{MockStream, script};
use crate::transport::Connection;
use engine_core::ids::MailboxId;
use engine_provider::Provider;

fn select_frag(tag: &str, validity: u32, uid_next: u32, exists: u32) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] x\r\n\
         * OK [UIDNEXT {uid_next}] x\r\n{tag} OK [READ-WRITE] done\r\n"
    )
}

fn fetch_frag(tag: &str, uids: &[u32]) -> String {
    let mut out = String::new();
    for (index, uid) in uids.iter().enumerate() {
        let seq = index + 1;
        write!(
            out,
            "* {seq} FETCH (UID {uid} FLAGS (\\Seen) \
             INTERNALDATE \"18-Mar-2026 10:00:00 +0000\" RFC822.SIZE 20 \
             ENVELOPE (NIL \"report {uid}\" ((\"A\" NIL \"alice\" \"test.local\")) NIL NIL \
             ((\"B\" NIL \"bob\" \"test.local\")) NIL NIL NIL \"<m{uid}@test.local>\"))\r\n"
        )
        .unwrap();
    }
    write!(out, "{tag} OK FETCH done\r\n").unwrap();
    out
}

const LIST_FRAG: &str = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                         * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                         a2 OK LIST done\r\n";

#[tokio::test]
async fn streamed_imap_sync_lands_in_the_store_with_progress() {
    // INBOX with 5 messages (UIDs 1..=5, UIDNEXT 6). Page size 2 → windows
    // 4:5, 2:3, 1:1 — newest first, three committed pages.
    let s3 = select_frag("a3", 100, 6, 5);
    let f4 = fetch_frag("a4", &[4, 5]);
    let s5 = select_frag("a5", 100, 6, 5);
    let f6 = fetch_frag("a6", &[2, 3]);
    let s7 = select_frag("a7", 100, 6, 5);
    let f8 = fetch_frag("a8", &[1]);
    let server = script(&[
        "* OK ready\r\n",
        "a1 OK LOGIN ok\r\n",
        LIST_FRAG,
        &s3,
        &f4,
        &s5,
        &f6,
        &s7,
        &f8,
    ]);

    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-acct").unwrap();

    let recorded: Mutex<Vec<SyncProgress>> = Mutex::new(Vec::new());
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        2,
        &|progress: SyncProgress| recorded.lock().unwrap().push(progress),
    )
    .await
    .expect("sync_mail_streamed");

    // Containers: both folders landed under the per-account folder-list scope.
    let mailbox_scope = provider.mailbox_scope(&account);
    let folders = store.object_keys(&mailbox_scope).await.unwrap();
    assert_eq!(folders.len(), 2, "INBOX + Archive");

    // Members: all five messages committed under the INBOX email scope.
    let email_scope = provider.email_scope(&account);
    let keys = store.object_keys(&email_scope).await.unwrap();
    assert_eq!(keys.len(), 5);
    assert_eq!(report.email.upserted, 5);
    // Identity is the synthesized (mailbox, UIDVALIDITY, UID) key.
    assert!(keys.iter().any(|k| k.as_str() == "imap:v100:u5@INBOX"));

    // Derived FTS rows make the synced mail searchable end to end.
    let hits = store
        .search_mail(
            core::slice::from_ref(&email_scope),
            &MailQuery::parse("subject:report").unwrap(),
            10,
        )
        .await
        .unwrap();
    assert!(!hits.hits.is_empty(), "FTS finds the synced subjects");

    // Progress: three committed pages, monotonic, ending at the full set against a
    // known denominator — a host could render mail before the sync finished.
    let seq = recorded.lock().unwrap();
    assert_eq!(seq.len(), 3, "one report per committed page");
    assert!(seq.iter().any(|p| p.fetched < 5), "an intermediate report");
    assert!(seq.windows(2).all(|w| w[0].fetched <= w[1].fetched));
    assert!(seq.iter().all(|p| p.scope == email_scope));
    assert_eq!(seq.last().unwrap().total, Some(5));
    assert_eq!(seq.last().unwrap().fetched, 5);
}
