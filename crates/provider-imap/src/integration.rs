//! Offline end-to-end: drive the IMAP `Provider` through `engine-sync`'s streaming
//! loop into a real `SqliteStore`, all over a mock stream (no Docker, no TLS).
//!
//! This proves the adapter composes with the orchestrator exactly like the JMAP
//! client does: folder container before email members, each page committed and
//! host-visible as it lands, progress reported per page, and the derived FTS rows
//! making the mail searchable — the whole cycle the store contract prescribes.

use core::{fmt::Write as _, time::Duration};
use std::sync::Mutex;

use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::{Message, SystemKeyword},
    membership::Memberships,
    sync::SyncScope,
};
use engine_provider::Provider;
use engine_search::MailQuery;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{
    IgnoreCommits, StreamTuning, SyncCommit, fetch_message_body, sync_email_streamed,
    sync_mail_streamed,
};
use store_sqlite::SqliteStore;

use crate::{
    ImapProvider,
    mock::{MockStream, script},
    transport::Connection,
};

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

/// The folder list as a `LIST-STATUS` server answers it (RFC 5819) — the rows and
/// their unread counts in one round trip. The tests below set
/// `list_status_advertised` to match, so the folder list stays a single command and
/// the tags after it keep counting from `a3`.
const LIST_FRAG: &str = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                         * STATUS \"INBOX\" (UNSEEN 2)\r\n\
                         * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                         * STATUS \"Archive\" (UNSEEN 0)\r\n\
                         a2 OK LIST done\r\n";

/// A `SELECT (CONDSTORE)` fragment carrying a `HIGHESTMODSEQ` — what a QRESYNC
/// session opens the mailbox with.
fn select_condstore_frag(
    tag: &str,
    validity: u32,
    uid_next: u32,
    exists: u32,
    modseq: u64,
) -> String {
    format!(
        "* {exists} EXISTS\r\n* OK [UIDVALIDITY {validity}] x\r\n\
         * OK [UIDNEXT {uid_next}] x\r\n* OK [HIGHESTMODSEQ {modseq}] x\r\n\
         {tag} OK [READ-WRITE] done\r\n"
    )
}

#[tokio::test]
async fn streamed_imap_sync_lands_in_the_store_with_progress() {
    // INBOX with 5 messages (UIDs 1..=5, UIDNEXT 6). Fetch batch 2 → the backfill
    // descends in newest-first groups 4:5, 2:3, 1:1 — three streamed FETCH commands
    // over ONE SELECT (the mailbox stays selected), each a committed chunk. The
    // backfill streams metadata only (no preview-body hydration).
    let s3 = select_frag("a3", 100, 6, 5);
    let f4 = fetch_frag("a4", &[4, 5]);
    let f5 = fetch_frag("a5", &[2, 3]);
    let f6 = fetch_frag("a6", &[1]);
    let server = script(&[
        "* OK ready\r\n",
        "a1 OK LOGIN ok\r\n",
        LIST_FRAG,
        &s3,
        &f4,
        &f5,
        &f6,
    ]);

    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.negotiated = crate::capability::Negotiated::from_capabilities(&["LIST-STATUS".to_owned()]);
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-acct").unwrap();

    let recorded: Mutex<Vec<(usize, Option<usize>, SyncScope)>> = Mutex::new(Vec::new());
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        StreamTuning::new(2, 0),
        &|commit: &SyncCommit<'_>| {
            recorded
                .lock()
                .unwrap()
                .push((commit.fetched, commit.total, commit.scope.clone()));
        },
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
    assert_eq!(seq.len(), 3, "one report per committed chunk");
    assert!(
        seq.iter().any(|(fetched, ..)| *fetched < 5),
        "an intermediate report"
    );
    assert!(seq.windows(2).all(|w| w[0].0 <= w[1].0));
    assert!(seq.iter().all(|(_, _, scope)| *scope == email_scope));
    assert_eq!(seq.last().unwrap().1, Some(5));
    assert_eq!(seq.last().unwrap().0, 5);
}

#[tokio::test]
async fn a_cleared_cursor_resync_reconciles_expunged_mail() {
    // The `reset`/`clear_mail_cursors` contract on a non-QRESYNC server: a no-cursor
    // re-sync over an EXISTING store must tombstone rows the server no longer has. A
    // fresh backfill's completing chunk reconciles against the full present set, so the
    // expunged UID 3 is dropped even though the pass streams additively.
    let s2 = select_frag("a2", 100, 4, 3);
    let f3 = fetch_frag("a3", &[1, 2, 3]);
    // The re-sync: UID 3 was expunged server-side; only 1,2 come back.
    let s4 = select_frag("a4", 100, 4, 2);
    let f5 = fetch_frag("a5", &[1, 2]);
    let server = script(&["* OK ready\r\n", "a1 OK LOGIN ok\r\n", &s2, &f3, &s4, &f5]);
    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-acct").unwrap();
    let email_scope = provider.email_scope(&account);

    // First sync: three messages land.
    sync_email_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        StreamTuning::new(50, 0),
        &IgnoreCommits,
    )
    .await
    .expect("first sync");
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 3);

    // Simulate a reset: clear the cursor so the next sync is a no-cursor re-sync.
    store.clear_scope_cursor(&email_scope).await.unwrap();

    let applied = sync_email_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        StreamTuning::new(50, 0),
        &IgnoreCommits,
    )
    .await
    .expect("resync");

    // The expunged UID 3 was tombstoned; only 1 and 2 remain.
    assert_eq!(
        applied.tombstoned, 1,
        "the expunged message was reconciled away"
    );
    let keys = store.object_keys(&email_scope).await.unwrap();
    assert_eq!(keys.len(), 2);
    assert!(!keys.iter().any(|k| k.as_str() == "imap:v100:u3@INBOX"));
}

#[tokio::test]
async fn body_fetch_extracts_and_caches_through_the_engine() {
    // greeting, login, then one SELECT + `BODY.PEEK[]` for UID 5. The body is a
    // multipart/alternative so the extractor must walk it to the text part.
    let body = "From: alice@test.local\r\nSubject: report\r\n\
                Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
                --b\r\nContent-Type: text/plain\r\n\r\nThe quarterly numbers.\r\n\
                --b\r\nContent-Type: text/html\r\n\r\n<p>The quarterly numbers.</p>\r\n--b--\r\n";
    let select = select_frag("a2", 100, 6, 5);
    let fetch = format!(
        "* 5 FETCH (UID 5 BODY[] {{{}}}\r\n{body})\r\na3 OK FETCH done\r\n",
        body.len()
    );
    let server = script(&["* OK ready\r\n", "a1 OK LOGIN ok\r\n", &select, &fetch]);

    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-acct").unwrap();
    let message = Message::new(
        MessageId::try_from("imap:v100:u5@INBOX").unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    );

    // First read fetches over IMAP, caches the raw, and extracts the text + HTML.
    let first = fetch_message_body(&provider, &store, &account, &message)
        .await
        .expect("fetch body");
    assert!(first.plain().unwrap().contains("The quarterly numbers."));
    assert!(
        first
            .html()
            .unwrap()
            .contains("<p>The quarterly numbers.</p>")
    );

    // Second read is served from the store's blob cache: the mock script is now
    // exhausted, so a network round trip would error — proving no re-fetch.
    let second = fetch_message_body(&provider, &store, &account, &message)
        .await
        .expect("fetch body from cache");
    assert_eq!(second.plain(), first.plain());
}

#[tokio::test]
async fn a_qresync_delta_reconciles_flags_and_expunges_in_the_store() {
    // A QRESYNC session: first a snapshot (records the modseq baseline), then a
    // `CHANGEDSINCE … VANISHED` delta that re-flags UID 1 and expunges UID 2. The
    // store must reflect both — the flag update *and* the tombstone — with no
    // re-snapshot, proving the incremental path end to end.
    // First sync (a QRESYNC backfill): one SELECT (condstore) + one streamed FETCH
    // group over UIDs 1..=3, metadata only (no preview hydration on the backfill).
    let snap_select = select_condstore_frag("a3", 100, 4, 3, 10);
    let snap_fetch = fetch_frag("a4", &[1, 2, 3]);
    // The delta: a SELECT (condstore, fresh modseq), then the CHANGEDSINCE fetch.
    let delta_select = select_condstore_frag("a5", 100, 4, 2, 15);
    // The delta. UIDNEXT has not moved (4), so nothing arrived and the pass is the
    // state half alone: UID 2 vanished, UID 1 came back `\Flagged`. There is no
    // envelope here because none was asked for — this is the whole point, and it is
    // why no body fetch follows either.
    let delta_changes = "* VANISHED (EARLIER) 2\r\n\
         * 1 FETCH (UID 1 FLAGS (\\Seen \\Flagged) MODSEQ (15))\r\n\
         a6 OK UID FETCH completed\r\n";
    let server = script(&[
        "* OK ready\r\n",
        "a1 OK LOGIN ok\r\n",
        LIST_FRAG,
        &snap_select,
        &snap_fetch,
        &delta_select,
        delta_changes,
    ]);

    let (stream, _) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    conn.negotiated = crate::capability::Negotiated::from_capabilities(&["LIST-STATUS".to_owned()]);
    conn.force_enabled("QRESYNC");
    let provider = ImapProvider::with_connection(conn, MailboxId::try_from("INBOX").unwrap());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("imap-acct").unwrap();

    // First sync: three messages land, the cursor records the modseq baseline.
    sync_mail_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        StreamTuning::new(50, 0),
        &IgnoreCommits,
    )
    .await
    .expect("snapshot sync");

    let email_scope = provider.email_scope(&account);
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 3);

    // QRESYNC delta: reconciles the flag change and the expunge incrementally.
    let applied = sync_email_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("imap"),
        Duration::from_mins(5),
        StreamTuning::new(50, 0),
        &IgnoreCommits,
    )
    .await
    .expect("qresync delta sync");
    assert_eq!(
        applied.upserted, 0,
        "a flag change rewrites no message: it is state, not content"
    );
    assert_eq!(applied.tombstoned, 1, "the expunged message is tombstoned");

    // The store now holds exactly UID 1 (re-flagged) and UID 3 (untouched); the
    // expunged UID 2 is gone — without a full re-snapshot.
    let keys: Vec<String> = store
        .object_keys(&email_scope)
        .await
        .unwrap()
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|k| k == "imap:v100:u1@INBOX"));
    assert!(keys.iter().any(|k| k == "imap:v100:u3@INBOX"));
    assert!(
        !keys.iter().any(|k| k == "imap:v100:u2@INBOX"),
        "the expunged message is tombstoned, not lingering"
    );

    // UID 1 carries its new \Flagged keyword in the store — read from the message row, which is
    // where a keyword lives. The stored payload is the provider's word on the message's content
    // and deliberately carries no keywords at all, so asserting against it would pass only while
    // the flag happened not to have moved.
    let row = store
        .list_mail(
            core::slice::from_ref(&account),
            engine_store::MailSelector::Keys(&[ProviderKey::new("imap:v100:u1@INBOX").unwrap()]),
            usize::MAX,
        )
        .await
        .unwrap()
        .pop()
        .expect("UID 1 present");
    assert!(
        row.mail.flags.flagged(),
        "the delta applied the flag change"
    );
    assert!(
        row.keywords
            .iter()
            .any(|k| k.as_system() == Some(SystemKeyword::Flagged)),
        "and the keyword membership moved with it"
    );
    // The content the delta never sent is still there. A state change that wrote the
    // whole row would have blanked these, because the response carried no envelope —
    // which is exactly what the old whole-object path risked on an envelope-less row.
    assert_eq!(
        row.mail.subject.as_deref(),
        Some("report 1"),
        "the subject the delta never mentioned survives"
    );
    assert_eq!(
        row.mail.from_addr.as_deref(),
        Some("alice@test.local"),
        "and so does the sender"
    );
}
