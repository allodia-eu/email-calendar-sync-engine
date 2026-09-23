//! Gated live integration: one folder synced further back than its account, against the
//! Stalwart harness over IMAP.
//!
//! The Sent folder is deepened past the account's window. A first pass must fetch Sent's older
//! mail (the adapter's `UID SEARCH SINCE` under Sent's own window) and keep it through the
//! backfill's additive groups and completing reconcile, while the Inbox stays at the account's
//! window. The pass fetches one message per group, so the newer of the two older Sent messages
//! arrives in an additive group rather than in the completing reconcile. A second pass is a delta,
//! which carries no date filter at all: an older message arriving in Sent is kept, one arriving in
//! the Inbox is dropped.
//!
//! The mail is dated by planting it over JMAP (`receivedAt` becomes the `INTERNALDATE`), in the
//! scratch account `bob@`, whose mailboxes no other suite counts. Skips with no
//! `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
    sync::{MailboxWindows, SyncUpdate, SyncWindow},
    time::{CalendarDate, UtcDateTime},
};
use engine_provider::Provider;
use engine_store::{MailSelector, ManualClock, StoreRead, WorkerId};
use engine_sync::{IgnoreCommits, StreamTuning, sync_mail};
use provider_imap::{ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;
use tokio_rustls::client::TlsStream;

/// The token every subject this suite plants carries, so a rerun can find its residue.
const MARKER: &str = "mbwimap";

/// The date every planted message carries: before the account window, inside Sent's.
const PLANTED_AT: &str = "2020-03-02T09:00:00Z";

type LiveProvider = ImapProvider<TlsStream<tokio::net::TcpStream>>;

/// Destroys what this run planted, however the run ends.
struct Planted<'a> {
    harness: &'a Harness,
    auth: (&'a str, &'a str),
    ids: Vec<String>,
}

impl Planted<'_> {
    fn plant(&mut self, role: &str, subject: &str) {
        let id = self
            .harness
            .plant_dated_email_as(self.auth, role, &format!("{MARKER} {subject}"), PLANTED_AT)
            .expect("plant a dated message");
        self.ids.push(id);
    }
}

impl Drop for Planted<'_> {
    fn drop(&mut self) {
        let _ = self.harness.destroy_emails_as(self.auth, &self.ids);
    }
}

async fn connect(harness: &Harness, auth: (&str, &str), mailbox: &MailboxId) -> LiveProvider {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(harness.imap_addr.as_str(), host, auth.0, auth.1);
    ImapProvider::connect(
        &config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
        mailbox.clone(),
    )
    .await
    .expect("connect IMAP")
}

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

/// The subjects this suite planted that the store now holds.
async fn held(store: &SqliteStore<ManualClock>, account: &AccountId) -> Vec<String> {
    let mut subjects: Vec<String> = store
        .list_mail(
            core::slice::from_ref(account),
            MailSelector::Newest,
            usize::MAX,
        )
        .await
        .expect("list mail")
        .into_iter()
        .filter_map(|row| row.mail.subject)
        .filter(|subject| subject.starts_with(MARKER))
        .collect();
    subjects.sort();
    subjects
}

#[tokio::test]
async fn a_deepened_sent_folder_reaches_back_and_no_other_folder_does() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping a_deepened_sent_folder_reaches_back_...: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let scratch = harness.scratch[0].clone();
    let auth = scratch.auth();
    let residue = harness
        .emails_with_subject_as(auth, MARKER)
        .expect("find residue");
    harness
        .destroy_emails_as(auth, &residue)
        .expect("clear residue of an earlier run");
    let mut planted = Planted {
        harness: &harness,
        auth,
        ids: Vec::new(),
    };
    planted.plant("sent", "sent before a");
    planted.plant("sent", "sent before b");
    planted.plant("inbox", "inbox before");

    let account = AccountId::try_from("live-window-imap").unwrap();
    let inbox = MailboxId::try_from("INBOX").unwrap();
    let first = connect(&harness, auth, &inbox).await;
    let SyncUpdate::Snapshot { objects, .. } = first
        .sync_mailboxes(&account, None)
        .await
        .expect("folders")
        .update
    else {
        panic!("a folder list is a snapshot");
    };
    let sent = objects
        .iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Sent))
        .map_or_else(|| panic!("the account's Sent folder"), |m| m.id.clone());
    let providers = vec![first, connect(&harness, auth, &sent).await];

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-09-01T00:00:00Z".parse().unwrap()))
            .expect("store");
    let windows = MailboxWindows::new(since(2025, 1, 1)).deepen(sent.clone(), since(2019, 1, 1));
    let pass = || {
        sync_mail(
            &providers,
            &store,
            &account,
            WorkerId::new("imap-live-window"),
            Duration::from_mins(5),
            StreamTuning::new(1, 1).within(windows.clone()),
            &IgnoreCommits,
        )
    };

    let report = pass().await;
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        held(&store, &account).await,
        vec![
            format!("{MARKER} sent before a"),
            format!("{MARKER} sent before b")
        ],
        "Sent was fetched back to its own window and the Inbox was not"
    );

    // A delta carries no date filter, so the admission is what decides.
    planted.plant("sent", "sent after");
    planted.plant("inbox", "inbox after");
    let report = pass().await;
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        held(&store, &account).await,
        vec![
            format!("{MARKER} sent after"),
            format!("{MARKER} sent before a"),
            format!("{MARKER} sent before b")
        ],
        "an older arrival is kept in Sent and dropped from the Inbox"
    );

    // The store reports how far back it holds Sent, and reads it over a span.
    let at = |text: &str| text.parse::<UtcDateTime>().unwrap();
    assert_eq!(
        store.oldest_in_mailbox(&account, &sent).await.unwrap(),
        Some(at(PLANTED_AT))
    );
    let span = store
        .list_mail(
            core::slice::from_ref(&account),
            MailSelector::Mailbox {
                mailbox: &sent,
                since: at("2020-01-01T00:00:00Z"),
                until: at("2021-01-01T00:00:00Z"),
            },
            usize::MAX,
        )
        .await
        .unwrap();
    assert_eq!(span.len(), 3, "the planted Sent messages, and nothing else");
}
