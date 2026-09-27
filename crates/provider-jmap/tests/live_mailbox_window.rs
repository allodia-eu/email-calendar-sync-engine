//! Gated live integration: one mailbox held further back than its account, against the
//! Stalwart harness over JMAP.
//!
//! JMAP syncs the whole account's mail as one `Email` scope, so the adapter fetches under the
//! account's window (`store-and-sync.md`, known gap): what a deeper Sent changes here is what a
//! **delta** keeps. `Email/changes` takes no filter, so an older message arriving after the
//! first pass is reported whatever its date, and the admission decides by the mailboxes it is
//! filed in: kept in Sent, dropped from the Inbox.
//!
//! The mail is dated by planting it with `receivedAt`, in the scratch account `bob@`, whose
//! mailboxes no other suite counts. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::Duration as StdDuration;

use engine_core::{
    ids::AccountId,
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

/// The token every subject this suite plants carries, so a rerun can find its residue.
const MARKER: &str = "mbwjmap";

/// The date every planted message carries: before the account window, inside Sent's.
const PLANTED_AT: &str = "2020-03-02T09:00:00Z";

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
async fn an_older_arrival_is_kept_only_in_the_deepened_mailbox() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping an_older_arrival_is_kept_only_...: STALWART_HTTP_ADDR unset");
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

    let account = AccountId::try_from("live-window-jmap").unwrap();
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(auth.0, auth.1),
    ))
    .await
    .expect("connect JMAP");
    let sent = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("mailboxes")
        .update
        .changed()
        .iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Sent))
        .map_or_else(|| panic!("the account's Sent mailbox"), |m| m.id.clone());

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-09-01T00:00:00Z".parse().unwrap()))
            .expect("store");
    let windows = MailboxWindows::new(since(2025, 1, 1)).deepen(sent.clone(), since(2019, 1, 1));
    let pass = || {
        sync_mail(
            core::slice::from_ref(&provider),
            &store,
            &account,
            WorkerId::new("jmap-live-window"),
            Duration::from_mins(5),
            StreamTuning::new(0, 0).within(windows.clone()),
            &IgnoreCommits,
        )
    };

    // The first pass is the snapshot; it establishes the cursor the delta runs from.
    let report = pass().await;
    assert!(report.is_ok(), "{report:?}");
    assert!(held(&store, &account).await.is_empty());

    let mut planted = Planted {
        harness: &harness,
        auth,
        ids: Vec::new(),
    };
    planted.plant("sent", "sent after");
    planted.plant("inbox", "inbox after");
    let report = pass().await;
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        held(&store, &account).await,
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
