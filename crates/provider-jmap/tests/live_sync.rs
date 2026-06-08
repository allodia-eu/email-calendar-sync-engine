//! Gated live integration: the **full sync loop** against the Stalwart harness.
//!
//! Drives `engine-sync` with the real `JmapProvider` into a real `SqliteStore`,
//! then asserts the seed invariants *in the store* (membership, the COPY/MOVE,
//! the duplicate `Message-ID`, keywords), proves the derived rows are searchable,
//! and that calendar events materialize occurrences end to end. Skips with no
//! `STALWART_HTTP_ADDR`, so the offline `cargo test --workspace` stays green.
//!
//! Per the determinism rule, every assertion is on harness-controlled content
//! (roles, names, subjects, `Message-ID`s, UIDs, counts) — never on the
//! server-assigned opaque ids — and counts that the submission tests mutate
//! (e.g. `Sent`) are avoided.

use core::time::Duration;
use std::sync::Mutex;
use std::time::Duration as StdDuration;

use engine_core::ids::AccountId;
use engine_core::mail::{Keyword, Mailbox, MailboxRole, Message};
use engine_core::sync::SyncScope;
use engine_core::time::TimeZoneId;
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_search::MailQuery;
use engine_store::{ManualClock, StoreRead, WorkerId};
use engine_sync::{SyncProgress, sync_calendar, sync_mail, sync_mail_streamed};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use serde::de::DeserializeOwned;
use stalwart_harness::Harness;
use store_sqlite::SqliteStore;

async fn load<T: DeserializeOwned>(
    store: &SqliteStore<ManualClock>,
    scope: &SyncScope,
    key: &engine_core::ids::ProviderKey,
) -> T {
    let payload = store
        .object_payload(scope, key)
        .await
        .unwrap()
        .expect("object present");
    serde_json::from_value(payload).expect("deserialize stored object")
}

// One cohesive end-to-end flow (sync mail → assert seed → search → sync calendar
// → assert occurrences); splitting it would obscure the single live scenario.
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end live scenario reads best whole"
)]
#[tokio::test]
async fn full_mail_and_calendar_sync_loop() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping full_mail_and_calendar_sync_loop: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("live-acct").unwrap();

    // ---- Mail: run the loop, then assert the seed in the store. ----
    sync_mail(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
    )
    .await
    .expect("sync_mail");

    let mailbox_scope = provider.mailbox_scope(&account);
    let email_scope = provider.email_scope(&account);

    // Resolve mailboxes by role/name from the synced containers, never by id.
    let (mut inbox, mut archive, mut projects) = (None, None, None);
    for key in store.object_keys(&mailbox_scope).await.unwrap() {
        let mailbox: Mailbox = load(&store, &mailbox_scope, &key).await;
        if mailbox.role == Some(MailboxRole::Inbox) {
            inbox = Some(mailbox.id.clone());
        }
        if mailbox.name == "Archive" {
            archive = Some(mailbox.id.clone());
        }
        if mailbox.name == "Projects" {
            projects = Some(mailbox.id.clone());
        }
    }
    let inbox = inbox.expect("inbox-role mailbox");
    let archive = archive.expect("Archive mailbox");
    let projects = projects.expect("Projects mailbox");

    let mut messages = Vec::new();
    for key in store.object_keys(&email_scope).await.unwrap() {
        messages.push(load::<Message>(&store, &email_scope, &key).await);
    }

    // 8 messages in the inbox.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.mailboxes.contains(&inbox))
            .count(),
        8
    );
    // The COPY: exactly one object in both inbox and Archive.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.mailboxes.contains(&inbox) && m.mailboxes.contains(&archive))
            .count(),
        1
    );
    // The MOVE: exactly one object in Projects and not the inbox.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.mailboxes.contains(&projects) && !m.mailboxes.contains(&inbox))
            .count(),
        1
    );
    // The duplicate Message-ID is two distinct stored objects.
    let dup: Vec<&Message> = messages
        .iter()
        .filter(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == "shared-dup-msgid@example.com")
        })
        .collect();
    assert_eq!(dup.len(), 2);
    assert_ne!(dup[0].id, dup[1].id);
    // The custom keyword survived the full loop.
    assert!(
        messages
            .iter()
            .any(|m| m.has_keyword(&Keyword::new("harness").unwrap()))
    );

    // Search end-to-end: the derived FTS rows make the baseline subject findable.
    let results = store
        .search_mail(
            &[email_scope],
            &MailQuery::parse("subject:baseline").unwrap(),
            10,
        )
        .await
        .unwrap();
    assert!(!results.hits.is_empty(), "FTS finds the baseline message");

    // ---- Calendar: run the loop, then assert events + occurrences. ----
    let horizon = Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2027-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    sync_calendar(
        &provider,
        &store,
        &account,
        WorkerId::new("live"),
        Duration::from_mins(5),
        horizon,
        &host_zone,
    )
    .await
    .expect("sync_calendar");

    let event_scope = provider.event_scope(&account);
    let event_keys = store.object_keys(&event_scope).await.unwrap();
    assert_eq!(event_keys.len(), 6, "6 seed events stored");

    // Every event materializes occurrences (the recurring one several), proving
    // JSCalendar recurrence flowed through normalize → expand → store.
    let mut total_occurrences = 0;
    for key in &event_keys {
        total_occurrences += store
            .index_row_counts(&event_scope, key)
            .await
            .unwrap()
            .occurrences;
    }
    assert!(
        total_occurrences >= 6,
        "expected at least one occurrence per event, got {total_occurrences}"
    );
}

/// Streams the mail seed three at a time, proving pages commit incrementally (a
/// host sees recent mail before the sync finishes) and progress is reported per
/// committed page. Robust to the submission tests adding a `Sent` copy: it asserts
/// relationships (every queried email committed, progress reached the stored set),
/// not a fixed seed count.
#[tokio::test]
async fn streamed_mail_sync_commits_pages_and_reports_progress() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping streamed_mail_sync_commits_pages_and_reports_progress: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-06-08T00:00:00Z".parse().unwrap()))
            .expect("store");
    let account = AccountId::try_from("live-stream").unwrap();

    // Page size 3 over the (≥9) seed forces several pages; the closure sink records
    // the running progress after each committed page.
    let recorded: Mutex<Vec<SyncProgress>> = Mutex::new(Vec::new());
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account,
        WorkerId::new("live-stream"),
        Duration::from_mins(5),
        3,
        &|progress: SyncProgress| recorded.lock().unwrap().push(progress),
    )
    .await
    .expect("sync_mail_streamed");

    let email_scope = provider.email_scope(&account);
    let stored = store.object_keys(&email_scope).await.unwrap().len();
    assert!(
        stored >= 9,
        "at least the nine-email seed is stored, got {stored}"
    );
    assert_eq!(
        report.email.upserted, stored,
        "every queried email was committed across the pages"
    );

    let seq = recorded.lock().unwrap();
    // Several pages committed incrementally, each firing one progress report.
    assert!(
        seq.len() >= 2,
        "expected several committed pages, got {}",
        seq.len()
    );
    // An intermediate report saw fewer than the full set — proof a host could
    // render mail mid-sync rather than only at the end.
    assert!(
        seq.iter().any(|p| p.fetched < stored),
        "expected an intermediate, pre-final progress report"
    );
    // Progress is monotonic, scoped to email, and ends at the full stored set
    // against a known denominator.
    assert!(seq.windows(2).all(|w| w[0].fetched <= w[1].fetched));
    assert!(seq.iter().all(|p| p.scope == email_scope));
    assert_eq!(seq.last().unwrap().total, Some(stored));
    assert_eq!(seq.last().unwrap().fetched, stored);
}
