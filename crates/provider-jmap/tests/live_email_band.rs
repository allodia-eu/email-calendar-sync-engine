//! Gated live integration: widening the account's mail by band over JMAP, against the Stalwart
//! harness (`docs/design/windows-and-widening.md`, §9).
//!
//! This is the first place the engine sends `Email/query` with `before`. Mail is planted with a
//! chosen `receivedAt` in the scratch account `bob@`, under a marker of each test's own, and
//! destroyed afterwards. A first pass holds three months; widening to six must fetch only the
//! band, counted from the commits marked as a widening: `after` and `before` cut on the same UTC
//! instant, so a message one second before the old floor is the band's and one on it the held
//! window's, and neither is listed twice. The store must end as a cold pass under six months
//! would, and a flag change and a destroy made to band mail afterwards must arrive by the next
//! delta, so the band kept the `Email` state. The second test kills a band mid-walk, destroys the
//! message its stored anchor names, files one in the gap after it, and resumes. Skips with no
//! `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::{collections::BTreeSet, sync::Mutex, time::Duration as StdDuration};

use engine_core::{
    ids::{AccountId, ProviderKey},
    mail::Keyword,
    sync::{MailboxWindows, StampedCursor, SyncWindow},
    time::CalendarDate,
};
use engine_provider::{MailEdit, Provider};
use engine_store::{MailSelector, ManualClock, Store, StoreRead, WorkerId};
use engine_sync::{MailSyncReport, StreamTuning, SyncCommit, sync_mail};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

/// Destroys what a run planted, however it ends.
struct Planted<'a> {
    harness: &'a Harness,
    marker: &'static str,
    ids: Vec<String>,
}

impl Planted<'_> {
    fn plant(&mut self, subject: &str, at: &str) -> String {
        let id = self
            .harness
            .plant_dated_email_as(
                self.harness.scratch[0].auth(),
                "inbox",
                &format!("{} {subject}", self.marker),
                at,
            )
            .expect("plant a dated message");
        self.ids.push(id.clone());
        id
    }
}

impl Drop for Planted<'_> {
    fn drop(&mut self) {
        let _ = self
            .harness
            .destroy_emails_as(self.harness.scratch[0].auth(), &self.ids);
    }
}

fn harness(marker: &str) -> Option<Harness> {
    let harness = Harness::from_env()?;
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let auth = harness.scratch[0].auth();
    let residue = harness
        .emails_with_subject_as(auth, marker)
        .expect("residue");
    harness
        .destroy_emails_as(auth, &residue)
        .expect("clear residue of an earlier run");
    Some(harness)
}

async fn connect(harness: &Harness) -> JmapProvider {
    let auth = harness.scratch[0].auth();
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(auth.0, auth.1),
        engine_http::test_server::user_agent(),
    ))
    .await
    .expect("connect JMAP")
}

fn account() -> AccountId {
    AccountId::try_from("live-band-jmap").unwrap()
}

fn since(month: u8) -> MailboxWindows {
    SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap()).into()
}

fn open_store() -> (SqliteStore<ManualClock>, ManualClock) {
    let clock = ManualClock::new("2026-07-01T00:00:00Z".parse().unwrap());
    (SqliteStore::open_in_memory(clock.clone()).unwrap(), clock)
}

/// The marked subjects each kind of commit carried: the widening's, and the rest.
#[derive(Default)]
struct Seen {
    band: Vec<String>,
    other: Vec<String>,
}

fn marked<'a>(commit: &'a SyncCommit<'_>, marker: &'a str) -> impl Iterator<Item = String> + 'a {
    commit
        .upserted
        .iter()
        .filter_map(|message| message.envelope.subject.clone())
        .filter(move |subject| subject.starts_with(marker))
}

async fn pass(
    provider: &JmapProvider,
    store: &SqliteStore<ManualClock>,
    window: MailboxWindows,
    marker: &str,
    seen: &Mutex<Seen>,
    batch: usize,
) -> MailSyncReport {
    let observer = |commit: &SyncCommit<'_>| {
        let mut seen = seen.lock().unwrap();
        let subjects = marked(commit, marker);
        if commit.widening {
            seen.band.extend(subjects);
        } else {
            seen.other.extend(subjects);
        }
    };
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        WorkerId::new("w-band-jmap"),
        Duration::from_secs(60),
        StreamTuning::new(batch, 0).within(window),
        &observer,
    )
    .await
}

/// The marked mail the store holds: subject, key and keywords, by subject.
async fn held(
    store: &SqliteStore<ManualClock>,
    marker: &str,
) -> Vec<(String, ProviderKey, Vec<Keyword>)> {
    let mut rows: Vec<_> = store
        .list_mail(&[account()], MailSelector::Newest, usize::MAX)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|row| {
            let subject = row.mail.subject?;
            subject
                .starts_with(marker)
                .then_some((subject, row.mail.key, row.keywords))
        })
        .collect();
    rows.sort();
    rows
}

fn names(marker: &str, subjects: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = subjects.iter().map(|s| format!("{marker} {s}")).collect();
    names.sort();
    names
}

fn sorted(mut subjects: Vec<String>) -> Vec<String> {
    subjects.sort();
    subjects
}

#[tokio::test]
async fn a_widening_walks_only_the_band_cut_on_the_floors_instant_and_keeps_the_state() {
    const MARKER: &str = "jmapband";
    let Some(harness) = harness(MARKER) else {
        eprintln!("skipping a_widening_walks_only_the_band_…: STALWART_HTTP_ADDR unset");
        return;
    };
    let mut planted = Planted {
        harness: &harness,
        marker: MARKER,
        ids: Vec::new(),
    };
    planted.plant("new", "2026-06-10T09:00:00Z");
    planted.plant("floor", "2026-04-01T00:00:00Z");
    planted.plant("eve", "2026-03-31T23:59:59Z");
    planted.plant("band", "2026-02-15T12:00:00Z");
    planted.plant("old", "2025-06-01T12:00:00Z");
    let provider = connect(&harness).await;
    let (store, _) = open_store();
    let seen = Mutex::new(Seen::default());

    assert!(
        pass(&provider, &store, since(4), MARKER, &seen, 1)
            .await
            .is_ok()
    );
    let widened = pass(&provider, &store, since(1), MARKER, &seen, 1).await;
    assert!(widened.is_ok(), "{widened:?}");
    assert!(widened.folders[0].reached_window);

    let seen = seen.into_inner().unwrap();
    assert_eq!(sorted(seen.other), names(MARKER, &["floor", "new"]));
    assert_eq!(
        sorted(seen.band),
        names(MARKER, &["band", "eve"]),
        "the band read held mail again, or missed its own"
    );
    let scope = provider.email_scope(&account());
    let stamped = StampedCursor::parse(
        store
            .load_sync_state(account(), &scope)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(stamped.covered(), &since(1));
    assert_eq!(stamped.target(), None);

    // The equivalence oracle: a cold pass under six months holds the same.
    let (cold, _) = open_store();
    assert!(
        pass(&provider, &cold, since(1), MARKER, &Mutex::default(), 2)
            .await
            .is_ok()
    );
    let strip = |rows: Vec<(String, ProviderKey, Vec<Keyword>)>| -> Vec<(String, ProviderKey)> {
        rows.into_iter().map(|(s, k, _)| (s, k)).collect()
    };
    assert_eq!(
        strip(held(&store, MARKER).await),
        strip(held(&cold, MARKER).await)
    );

    // The band kept the state: a flag on band mail and a destroy of it arrive by delta.
    let rows = held(&store, MARKER).await;
    let key = |name: &str| {
        let subject = format!("{MARKER} {name}");
        rows.iter()
            .find(|(held, ..)| *held == subject)
            .map_or_else(|| panic!("{subject} is held"), |(_, key, _)| key.clone())
    };
    let (flagged, destroyed) = (key("band"), key("eve"));
    let flag = Keyword::new("$flagged").unwrap();
    provider
        .edit_mail(
            &account(),
            &MailEdit::SetKeywords {
                target: flagged.clone(),
                add: BTreeSet::from([flag.clone()]),
                remove: BTreeSet::new(),
            },
        )
        .await
        .expect("flag band mail");
    provider
        .edit_mail(
            &account(),
            &MailEdit::Delete {
                target: destroyed.clone(),
            },
        )
        .await
        .expect("destroy band mail");
    assert!(
        pass(&provider, &store, since(1), MARKER, &Mutex::default(), 2)
            .await
            .is_ok()
    );
    let after = held(&store, MARKER).await;
    assert!(
        after
            .iter()
            .any(|(_, key, keywords)| *key == flagged && keywords.contains(&flag)),
        "the flag on band mail was lost"
    );
    assert!(
        after.iter().all(|(_, key, _)| *key != destroyed),
        "the destroy of band mail was lost"
    );
}

#[tokio::test]
async fn a_band_killed_mid_walk_resumes_past_a_destroyed_anchor_and_stores_the_gap() {
    const MARKER: &str = "jmapbandkill";
    let Some(harness) = harness(MARKER) else {
        eprintln!("skipping a_band_killed_mid_walk_…: STALWART_HTTP_ADDR unset");
        return;
    };
    let mut planted = Planted {
        harness: &harness,
        marker: MARKER,
        ids: Vec::new(),
    };
    planted.plant("new", "2026-06-10T09:00:00Z");
    for (n, day) in ["05", "04", "03", "02"].iter().enumerate() {
        planted.plant(&format!("b{n}"), &format!("2026-02-{day}T09:00:00Z"));
    }
    let provider = connect(&harness).await;
    let (store, clock) = open_store();
    assert!(
        pass(&provider, &store, since(4), MARKER, &Mutex::default(), 2)
            .await
            .is_ok()
    );

    // Killed once the walk's first page, two messages, has committed: the future is dropped.
    let seen = Mutex::new(Seen::default());
    let reached = tokio::sync::Notify::new();
    let observer = |commit: &SyncCommit<'_>| {
        let subjects: Vec<String> = marked(commit, MARKER).collect();
        if commit.widening && !subjects.is_empty() {
            seen.lock().unwrap().band.extend(subjects);
            reached.notify_one();
        }
    };
    let owner = account();
    let killed = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &owner,
        WorkerId::new("w-killed"),
        Duration::from_secs(60),
        StreamTuning::new(2, 0).within(since(1)),
        &observer,
    );
    tokio::select! {
        _ = killed => panic!("the band ended before it could be killed"),
        () = reached.notified() => {}
    }
    let before_kill = sorted(core::mem::take(&mut seen.lock().unwrap().band));
    assert_eq!(
        before_kill,
        names(MARKER, &["b0", "b1"]),
        "two messages a page"
    );
    let scope = provider.email_scope(&account());
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
        "claimed what the band had not fetched"
    );
    assert_eq!(mid.target(), Some(&since(1)));

    // The message the stored anchor names (the page's last, `b1`) is destroyed, and one is filed
    // in the gap after the message before it.
    let anchor = planted.ids[2].clone();
    harness
        .destroy_emails_as(harness.scratch[0].auth(), core::slice::from_ref(&anchor))
        .expect("destroy the anchor");
    planted.plant("gap", "2026-02-04T18:00:00Z");

    clock.advance(Duration::from_secs(120));
    let resumed = Mutex::new(Seen::default());
    assert!(
        pass(&provider, &store, since(1), MARKER, &resumed, 2)
            .await
            .is_ok()
    );
    let after = sorted(resumed.into_inner().unwrap().band);
    eprintln!("before the kill {before_kill:?}, after {after:?}");
    assert_eq!(
        after,
        names(MARKER, &["b2", "b3", "gap"]),
        "the walk did not continue after the message before its destroyed anchor"
    );
    let subjects: Vec<String> = held(&store, MARKER)
        .await
        .into_iter()
        .map(|(s, ..)| s)
        .collect();
    assert_eq!(
        subjects,
        names(MARKER, &["b0", "b2", "b3", "gap", "new"]),
        "every band message stored once the band ends, the destroyed anchor gone"
    );
}
