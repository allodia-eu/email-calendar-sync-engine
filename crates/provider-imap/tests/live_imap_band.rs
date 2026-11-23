//! Gated live integration: widening a folder's window by band, against every configured IMAP
//! server (`docs/design/windows-and-widening.md`, §9).
//!
//! Mail is planted at chosen dates by a dated `APPEND` (`MailImporter`) into a folder created for
//! the run and deleted after it: on the floor's day at both ends of it, on the day before at both
//! ends, inside the band, and before it. A first pass holds three months; widening to six must
//! then fetch only the band and the old floor's day (counted from the commits marked as a
//! widening), miss nothing, and leave the store as a cold pass under six months would. A flag
//! change and an expunge on band mail after the widening must arrive by the next delta, so the band
//! kept the cursor. A second test kills the pass mid-band and resumes it.
//!
//! This is the first place the engine sends `UID SEARCH UID 1:n SINCE … BEFORE …`.
//!
//! On Stalwart it runs as the scratch account `carol@`; the Dovecot harness has only Alice, whose
//! seeded folders this leaves alone. `DOVECOT_AHEAD_IMAP_ADDR` and `DOVECOT_BEHIND_IMAP_ADDR` name
//! the harness image run with `TZ=Pacific/Kiritimati` (UTC+14) and `TZ=Etc/GMT+12` (UTC−12), where
//! the server's day and the engine's UTC day differ at a floor: the runs that showed a band, or a
//! first pass, ending exactly at a floor leaves a message out (`provider-imap/src/band.rs`). Skips
//! per server when its address variable is unset.

#![cfg(feature = "import")]

#[path = "common/imap_live.rs"]
mod imap_live;

use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, resume_unwind},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey},
    mail::{Keyword, Message, MessageMetadata},
    membership::Memberships,
    raw::RawMime,
    sync::{MailboxWindows, StampedCursor, SyncScope, SyncWindow},
    time::{CalendarDate, UtcDateTime},
};
use engine_provider::{
    MailEdit, MailboxEdit, MailboxWrites, Provider,
    import::{MailImporter, MessageImport},
};
use engine_store::{MailSelector, ManualClock, Store, StoreRead, WorkerId};
use engine_sync::{MailSyncReport, StreamTuning, SyncCommit, sync_mail};
use futures_util::FutureExt;
use imap_live::{DOVECOT_REV1, DOVECOT_REV2, LiveProvider, Server, connect_to};
use serde_json::Value;
use store_sqlite::SqliteStore;

const STALWART_SCRATCH: Server = Server {
    label: "stalwart",
    addr_var: "STALWART_IMAP_ADDR",
    account: "carol@test.local",
    password: "harness-carol-pw",
};

/// The Dovecot image run a day's width ahead of UTC (`TZ=Pacific/Kiritimati`, UTC+14).
const DOVECOT_AHEAD: Server = Server {
    label: "dovecot-utc+14",
    addr_var: "DOVECOT_AHEAD_IMAP_ADDR",
    account: "alice@test.local",
    password: "dovecot-alice-pw",
};

/// The Dovecot image run a day's width behind UTC (`TZ=Etc/GMT+12`, UTC−12).
const DOVECOT_BEHIND: Server = Server {
    label: "dovecot-utc-12",
    addr_var: "DOVECOT_BEHIND_IMAP_ADDR",
    account: "alice@test.local",
    password: "dovecot-alice-pw",
};

const SERVERS: [Server; 5] = [
    STALWART_SCRATCH,
    DOVECOT_REV1,
    DOVECOT_REV2,
    DOVECOT_AHEAD,
    DOVECOT_BEHIND,
];

/// What is planted, by subject, and when it arrived.
const PLANTED: [(&str, &str); 7] = [
    ("new", "2026-06-10T09:00:00Z"),
    ("floor-first", "2026-04-01T00:00:00Z"),
    ("floor-last", "2026-04-01T23:59:59Z"),
    ("eve-first", "2026-03-31T00:00:00Z"),
    ("eve-last", "2026-03-31T23:59:59Z"),
    ("band", "2026-02-15T12:00:00Z"),
    ("old", "2025-06-01T12:00:00Z"),
];

/// Whether `server` keeps a day other than UTC's.
fn zoned(server: &Server) -> bool {
    [DOVECOT_AHEAD.addr_var, DOVECOT_BEHIND.addr_var].contains(&server.addr_var)
}

fn account() -> AccountId {
    AccountId::try_from("live-band").unwrap()
}

fn since(month: u8) -> MailboxWindows {
    SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap()).into()
}

fn run_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn raw(subject: &str, run: u128) -> RawMime {
    RawMime::new(format!(
        "From: Sender <sender@example.com>\r\nTo: Receiver <receiver@example.com>\r\n\
         Subject: {subject}\r\nMessage-ID: <band-{subject}-{run}@example.com>\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nPlanted for run {run}.\r\n"
    ))
}

/// Creates a folder for the run, runs `body` in it, and deletes it however `body` ends.
async fn in_scratch_folder<F, Fut>(server: &Server, test: &str, body: F)
where
    F: FnOnce(MailboxId) -> Fut,
    Fut: Future<Output = ()>,
{
    let Some(admin) = connect_to(server, "INBOX", test).await else {
        return;
    };
    let label = server.label;
    let created = admin
        .edit_mailbox(
            &account(),
            &MailboxEdit::Create {
                name: format!("Band {}", run_id()),
                parent: None,
            },
        )
        .await
        .unwrap_or_else(|err| panic!("create the folder on {label}: {err}"));
    let folder = created.mailbox.expect("the create names its folder");
    let checked = AssertUnwindSafe(body(folder.clone())).catch_unwind().await;
    admin
        .edit_mailbox(&account(), &MailboxEdit::Delete { target: folder })
        .await
        .unwrap_or_else(|err| panic!("delete the folder on {label}: {err}"));
    if let Err(failed) = checked {
        resume_unwind(failed);
    }
}

async fn plant(provider: &LiveProvider, folder: &MailboxId, label: &str) {
    let run = run_id();
    let batch: Vec<MessageImport> = PLANTED
        .iter()
        .map(|(subject, at)| {
            MessageImport::new(raw(subject, run), Memberships::of_one(folder.clone()))
                .with_metadata(
                    MessageMetadata::default().with_received_at(at.parse::<UtcDateTime>().unwrap()),
                )
        })
        .collect();
    for answer in provider.import_messages(&account(), &batch).await {
        answer.unwrap_or_else(|err| panic!("plant on {label}: {err}"));
    }
}

/// The subjects each commit carried, split into the widening's and the rest.
#[derive(Default)]
struct Seen {
    band: Vec<String>,
    other: Vec<String>,
}

fn subjects(messages: &[Message]) -> impl Iterator<Item = String> + '_ {
    messages
        .iter()
        .filter_map(|message| message.envelope.subject.clone())
}

async fn pass(
    provider: &LiveProvider,
    store: &SqliteStore<ManualClock>,
    window: MailboxWindows,
    seen: &Mutex<Seen>,
) -> MailSyncReport {
    let observer = |commit: &SyncCommit<'_>| {
        let mut seen = seen.lock().unwrap();
        let into = if commit.widening {
            &mut seen.band
        } else {
            &mut seen.other
        };
        into.extend(subjects(commit.upserted));
    };
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        WorkerId::new("w-band"),
        Duration::from_secs(60),
        StreamTuning::new(1, 1).within(window),
        &observer,
    )
    .await
}

fn open_store() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = ManualClock::new("2026-07-01T00:00:00Z".parse().unwrap());
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

/// What the store holds for the folder: each key with its stored content, for the oracle.
async fn held(store: &SqliteStore<ManualClock>, scope: &SyncScope) -> Vec<(ProviderKey, Value)> {
    let mut held = store.scope_objects(scope).await.unwrap();
    held.sort_by(|a, b| a.0.cmp(&b.0));
    held
}

/// Each held message's subject, key and keywords.
async fn rows(store: &SqliteStore<ManualClock>) -> Vec<(String, ProviderKey, Vec<Keyword>)> {
    let mut rows: Vec<_> = store
        .list_mail(&[account()], MailSelector::Newest, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.mail.subject.unwrap_or_default(),
                row.mail.key,
                row.keywords,
            )
        })
        .collect();
    rows.sort();
    rows
}

fn sorted(mut subjects: Vec<String>) -> Vec<String> {
    subjects.sort();
    subjects
}

#[tokio::test]
async fn a_widening_fetches_only_the_band_its_days_meet_the_held_ones_and_it_keeps_the_cursor() {
    for server in &SERVERS {
        let test = "a_widening_fetches_only_the_band_…";
        in_scratch_folder(server, test, |folder| async move {
            let label = server.label;
            let reader = connect_to(server, folder.as_str(), test).await.unwrap();
            plant(&reader, &folder, label).await;
            let scope = reader.email_scope(&account());
            let (store, _) = open_store();
            let seen = Mutex::new(Seen::default());
            assert!(
                pass(&reader, &store, since(4), &seen).await.is_ok(),
                "{label}"
            );
            let first = sorted(core::mem::take(&mut seen.lock().unwrap().other));
            // Whatever the server's zone, a first pass holds exactly what the window admits by the
            // engine's UTC day: a server behind UTC dates the floor's first message the day before.
            assert_eq!(
                first,
                ["floor-first", "floor-last", "new"],
                "{label}: the first pass missed or kept mail by the server's day"
            );

            let widened = pass(&reader, &store, since(1), &seen).await;
            assert!(widened.is_ok(), "{label}: {widened:?}");
            assert!(widened.folders[0].reached_window, "{label}");
            let band = sorted(core::mem::take(&mut seen.lock().unwrap().band));
            eprintln!("{label}: held {first:?}, band {band:?}");

            // The band reaches one server day past the old floor, so the two share at most that
            // day's mail, and together hold everything from January on whatever the server's
            // zone: what the first pass fetched and the engine's UTC filter dropped included.
            assert!(
                !band.contains(&"new".to_owned()),
                "{label}: the band read past its day"
            );
            let union: BTreeSet<&String> = first.iter().chain(&band).collect();
            let wanted: BTreeSet<String> = PLANTED
                .iter()
                .filter(|(subject, _)| *subject != "old")
                .map(|(subject, _)| (*subject).to_owned())
                .collect();
            assert_eq!(
                union,
                wanted.iter().collect(),
                "{label}: a gap, or more than the window"
            );
            if !zoned(server) {
                // On a server keeping UTC days the floor's day is the only one listed twice.
                assert_eq!(
                    band,
                    ["band", "eve-first", "eve-last", "floor-first", "floor-last"],
                    "{label}"
                );
            }
            let stamped = StampedCursor::parse(
                store
                    .load_sync_state(account(), &scope)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(stamped.covered(), &since(1), "{label}");
            assert_eq!(stamped.target(), None, "{label}");

            // The equivalence oracle: a cold pass under six months holds the same.
            let (cold, _) = open_store();
            assert!(
                pass(&reader, &cold, since(1), &Mutex::default())
                    .await
                    .is_ok()
            );
            assert_eq!(
                held(&store, &scope).await,
                held(&cold, &scope).await,
                "{label}"
            );

            // The band kept the cursor: changes to band mail made after it arrive by delta.
            let rows_now = rows(&store).await;
            let by_subject = |subject: &str| {
                rows_now
                    .iter()
                    .find(|(held, ..)| held == subject)
                    .map_or_else(
                        || panic!("{label}: {subject} is held"),
                        |(_, key, _)| key.clone(),
                    )
            };
            let flagged = by_subject("band");
            let other = band
                .iter()
                .find(|subject| *subject != "band")
                .expect("more band mail");
            let expunged = by_subject(other);
            reader
                .edit_mail(
                    &account(),
                    &MailEdit::SetKeywords {
                        target: flagged.clone(),
                        add: BTreeSet::from([Keyword::new("$flagged").unwrap()]),
                        remove: BTreeSet::new(),
                    },
                )
                .await
                .unwrap_or_else(|err| panic!("flag on {label}: {err}"));
            reader
                .edit_mail(
                    &account(),
                    &MailEdit::Delete {
                        target: expunged.clone(),
                    },
                )
                .await
                .unwrap_or_else(|err| panic!("expunge on {label}: {err}"));
            assert!(
                pass(&reader, &store, since(1), &seen).await.is_ok(),
                "{label}"
            );
            let after = rows(&store).await;
            let flag = Keyword::new("$flagged").unwrap();
            assert!(
                after
                    .iter()
                    .any(|(_, key, keywords)| *key == flagged && keywords.contains(&flag)),
                "{label}: the flag on band mail was lost"
            );
            assert!(
                after.iter().all(|(_, key, _)| *key != expunged),
                "{label}: the expunge of band mail was lost"
            );
        })
        .await;
    }
}

#[tokio::test]
async fn a_widening_killed_mid_band_resumes_below_its_watermark_to_the_same_store() {
    for server in &SERVERS {
        let test = "a_widening_killed_mid_band_…";
        in_scratch_folder(server, test, |folder| async move {
            let label = server.label;
            let reader = connect_to(server, folder.as_str(), test).await.unwrap();
            plant(&reader, &folder, label).await;
            let scope = reader.email_scope(&account());
            let (store, clock) = open_store();
            assert!(
                pass(&reader, &store, since(4), &Mutex::default())
                    .await
                    .is_ok()
            );

            // Killed once the band's first chunk has committed: the future is dropped mid-pass.
            let seen = Mutex::new(Seen::default());
            let reached = Arc::new(tokio::sync::Notify::new());
            let killed = {
                let observer = |commit: &SyncCommit<'_>| {
                    if commit.widening {
                        seen.lock().unwrap().band.extend(subjects(commit.upserted));
                        reached.notify_one();
                    }
                };
                // One UID a group, one chunk a group: every band commit checkpoints.
                let owner = account();
                let sync = sync_mail(
                    core::slice::from_ref(&reader),
                    &store,
                    &owner,
                    WorkerId::new("w-killed"),
                    Duration::from_secs(60),
                    StreamTuning::new(1, 0).within(since(1)),
                    &observer,
                );
                tokio::select! {
                    _ = sync => panic!("{label}: the band ended before it could be killed"),
                    () = reached.notified() => {}
                }
                core::mem::take(&mut seen.lock().unwrap().band)
            };
            let mid = StampedCursor::parse(
                store
                    .load_sync_state(account(), &scope)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(
                mid.covered(),
                &since(4),
                "{label}: claimed what it did not fetch"
            );
            assert_eq!(mid.target(), Some(&since(1)), "{label}");
            assert!(
                mid.cursor().as_str().contains(";w"),
                "{label}: no band watermark"
            );

            // The killed pass's lease lapses, and the next pass continues the band.
            clock.advance(Duration::from_secs(120));
            let resumed = Mutex::new(Seen::default());
            assert!(
                pass(&reader, &store, since(1), &resumed).await.is_ok(),
                "{label}"
            );
            let rest = resumed.into_inner().unwrap().band;
            eprintln!("{label}: before the kill {killed:?}, after {rest:?}");
            assert!(
                killed.iter().all(|subject| !rest.contains(subject)),
                "{label}: the band started over"
            );

            let (cold, _) = open_store();
            assert!(
                pass(&reader, &cold, since(1), &Mutex::default())
                    .await
                    .is_ok()
            );
            assert_eq!(
                held(&store, &scope).await,
                held(&cold, &scope).await,
                "{label}"
            );
            let ended = StampedCursor::parse(
                store
                    .load_sync_state(account(), &scope)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            assert_eq!(ended.covered(), &since(1), "{label}");
        })
        .await;
    }
}
