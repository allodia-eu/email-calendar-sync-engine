//! Gated live integration: one mailbox held further back than its account, against the
//! Stalwart harness over JMAP.
//!
//! JMAP syncs the whole account's mail as one `Email` scope. A snapshot asks for the account's
//! window and, beside it, the deeper mailbox's own (`email_window.rs`), and deepening a mailbox
//! after a first pass widens that one scope by a band of that mailbox's older mail
//! (`window_cursor`, `email_band.rs`), so Sent's older mail arrives on the next pass with no
//! cursor cleared. A **delta** takes no filter, so an older message
//! arriving later is reported whatever its date, and the admission decides by the mailboxes it
//! is filed in: kept in Sent, dropped from the Inbox.
//!
//! The mail is dated by planting it with `receivedAt`, in the scratch account `bob@`, whose
//! mailboxes no other suite counts. Each test plants under a marker of its own, so the two can
//! run side by side. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId},
    mail::MailboxRole,
    sync::{MailboxWindows, SyncWindow},
    time::{CalendarDate, UtcDateTime},
};
use engine_provider::Provider;
use engine_store::{MailSelector, ManualClock, StoreRead, WorkerId};
use engine_sync::{IgnoreCommits, StreamTuning, sync_mail};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

/// The date every planted message carries: before the account window, inside Sent's.
const PLANTED_AT: &str = "2020-03-02T09:00:00Z";

/// Destroys what this run planted, however the run ends.
struct Planted<'a> {
    harness: &'a Harness,
    auth: (&'a str, &'a str),
    marker: &'static str,
    ids: Vec<String>,
}

impl Planted<'_> {
    fn plant(&mut self, role: &str, subject: &str) {
        let subject = format!("{} {subject}", self.marker);
        let id = self
            .harness
            .plant_dated_email_as(self.auth, role, &subject, PLANTED_AT)
            .expect("plant a dated message");
        self.ids.push(id);
    }
}

impl Drop for Planted<'_> {
    fn drop(&mut self) {
        let _ = self.harness.destroy_emails_as(self.auth, &self.ids);
    }
}

fn since(year: i32, month: u8, day: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(year, month, day).unwrap())
}

/// The subjects under `marker` the store now holds.
async fn held(store: &SqliteStore<ManualClock>, account: &AccountId, marker: &str) -> Vec<String> {
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
        .filter(|subject| subject.starts_with(&format!("{marker} ")))
        .collect();
    subjects.sort();
    subjects
}

/// The harness, ready, with the residue of an earlier run under `marker` cleared.
fn harness(marker: &str) -> Option<Harness> {
    let harness = Harness::from_env()?;
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let auth = harness.scratch[0].auth();
    let residue = harness
        .emails_with_subject_as(auth, marker)
        .expect("find residue");
    harness
        .destroy_emails_as(auth, &residue)
        .expect("clear residue of an earlier run");
    Some(harness)
}

/// A JMAP provider for the scratch account, and that account's Sent mailbox.
async fn connect(harness: &Harness, account: &AccountId) -> (JmapProvider, MailboxId) {
    let auth = harness.scratch[0].auth();
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(auth.0, auth.1),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect JMAP");
    let sent = provider
        .sync_mailboxes(account, None)
        .await
        .expect("mailboxes")
        .update
        .changed()
        .iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Sent))
        .map_or_else(|| panic!("the account's Sent mailbox"), |m| m.id.clone());
    (provider, sent)
}

async fn pass(
    provider: &JmapProvider,
    store: &SqliteStore<ManualClock>,
    account: &AccountId,
    windows: MailboxWindows,
) {
    let report = sync_mail(
        core::slice::from_ref(provider),
        store,
        account,
        WorkerId::new("jmap-live-window"),
        Duration::from_mins(5),
        StreamTuning::new(0, 0).within(windows),
        &IgnoreCommits,
    )
    .await;
    assert!(report.is_ok(), "{report:?}");
}

fn store() -> SqliteStore<ManualClock> {
    SqliteStore::open_in_memory(ManualClock::new("2026-09-01T00:00:00Z".parse().unwrap()))
        .expect("store")
}

#[tokio::test]
async fn an_older_arrival_is_kept_only_in_the_deepened_mailbox() {
    const MARKER: &str = "mbwjmap";
    let Some(harness) = harness(MARKER) else {
        eprintln!("skipping an_older_arrival_is_kept_only_...: STALWART_HTTP_ADDR unset");
        return;
    };
    let account = AccountId::try_from("live-window-jmap").unwrap();
    let (provider, sent) = connect(&harness, &account).await;
    let store = store();
    let windows = MailboxWindows::new(since(2025, 1, 1)).deepen(sent.clone(), since(2019, 1, 1));

    // The first pass is the snapshot; it establishes the cursor the delta runs from.
    pass(&provider, &store, &account, windows.clone()).await;
    assert_eq!(held(&store, &account, MARKER).await, [] as [String; 0]);

    let mut planted = Planted {
        harness: &harness,
        auth: harness.scratch[0].auth(),
        marker: MARKER,
        ids: Vec::new(),
    };
    planted.plant("sent", "sent after");
    planted.plant("inbox", "inbox after");
    pass(&provider, &store, &account, windows).await;
    assert_eq!(
        held(&store, &account, MARKER).await,
        vec![format!("{MARKER} sent after")],
        "an older arrival is kept in Sent and dropped from the Inbox"
    );

    let at = |text: &str| text.parse::<UtcDateTime>().unwrap();
    assert_eq!(
        store.oldest_in_mailbox(&account, &sent).await.unwrap(),
        Some(at(PLANTED_AT)),
        "the store reports how far back it holds Sent"
    );
}

#[tokio::test]
async fn deepening_sent_after_a_first_pass_fetches_its_older_mail_on_the_next() {
    const MARKER: &str = "mbwdeepjmap";
    let Some(harness) = harness(MARKER) else {
        eprintln!("skipping deepening_sent_after_a_first_pass_...: STALWART_HTTP_ADDR unset");
        return;
    };
    let mut planted = Planted {
        harness: &harness,
        auth: harness.scratch[0].auth(),
        marker: MARKER,
        ids: Vec::new(),
    };
    planted.plant("sent", "sent before");
    planted.plant("inbox", "inbox before");
    let account = AccountId::try_from("live-deepen-jmap").unwrap();
    let (provider, sent) = connect(&harness, &account).await;
    let store = store();
    let account_only = MailboxWindows::new(since(2025, 1, 1));

    pass(&provider, &store, &account, account_only.clone()).await;
    assert_eq!(
        held(&store, &account, MARKER).await,
        [] as [String; 0],
        "both are older than the account's window"
    );

    // Only the windows change: no cursor is cleared.
    pass(
        &provider,
        &store,
        &account,
        account_only.deepen(sent.clone(), since(2019, 1, 1)),
    )
    .await;
    assert_eq!(
        held(&store, &account, MARKER).await,
        vec![format!("{MARKER} sent before")],
        "Sent's older mail is fetched, and no other mailbox's"
    );
    let at = |text: &str| text.parse::<UtcDateTime>().unwrap();
    assert!(
        store
            .oldest_in_mailbox(&account, &sent)
            .await
            .unwrap()
            .is_some_and(|oldest| oldest <= at(PLANTED_AT)),
        "the store reaches back into Sent as far as the planted message"
    );
}
