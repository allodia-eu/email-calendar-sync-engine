//! The scenarios themselves. The simulated provider, fixtures and helpers they reach via
//! `super::` live in the crate root beside this file.
//!
//! Split out to keep both inside the 500-line limit. Reached through `#[path]`, because a
//! `tests/NAME.rs` *is* a crate root and a nested file is not a module of it by default.

use super::*;

#[tokio::test]
async fn cold_add_streams_newest_first_and_resumes_after_a_kill() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(9), 3);
    // The app is "killed" after two committed chunks (six messages).
    provider.fail_after(2);

    let view = Mutex::new(ClientView::default());
    let observer = |commit: &SyncCommit<'_>| view.lock().unwrap().apply(commit);

    // First run: the streamed sync surfaces mail chunk by chunk, then the kill aborts it.
    let killed = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &observer,
        )
        .await;
    assert!(
        !killed.is_ok(),
        "the mid-stream failure surfaces as an error"
    );
    // Yet the six committed messages are durable and already in the client's view —
    // the newest first (m0 is the newest).
    assert_eq!(view.lock().unwrap().len(), 6, "committed rows are visible");
    assert_eq!(
        view.lock().unwrap().subjects.get(&key("m0")),
        Some(&"Subject 0".to_owned()),
        "the newest message rendered first"
    );

    // Resume: a fresh, healthy connection picks up from the checkpoint — it must be
    // handed the watermark cursor, not restart from the newest.
    let resumed = SimProvider::new(messages(9), 3);
    let report = engine
        .sync_mail(
            core::slice::from_ref(&resumed),
            &account(),
            responsive(),
            &observer,
        )
        .await;
    assert_eq!(
        report.upserted(),
        3,
        "only the remaining three were fetched"
    );
    assert_eq!(
        *resumed.starts.lock().unwrap(),
        vec![6],
        "the resume started at the checkpoint (index 6), not 0"
    );
    // All nine are now present in the store and the client's view.
    assert_eq!(view.lock().unwrap().len(), 9);
    assert_eq!(
        engine.mail_window(&[account()], 100).await.unwrap().len(),
        9
    );
}

#[tokio::test]
async fn warm_start_paints_cached_mail_offline_then_syncs() {
    // A prior session synced the account; a fresh session opens the same store.
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5), 2);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;

    // Now offline. The warm-start read still paints the cached mail with no provider
    // call — the instant, offline-first list.
    provider.set_offline(true);
    let cached = engine.mail_window(&[account()], 50).await.unwrap();
    assert_eq!(cached.len(), 5, "cached mail renders offline");
    assert_eq!(engine.mailboxes(&account()).await.unwrap().len(), 1);

    // A background sync while offline degrades gracefully (an error the host ignores),
    // leaving the cached view intact.
    let offline_sync = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;
    assert!(
        !offline_sync.is_ok(),
        "an offline sync fails, it does not corrupt state"
    );
    assert_eq!(engine.mail_window(&[account()], 50).await.unwrap().len(), 5);

    // Back online, a sync reconciles without disturbing the cache count.
    provider.set_offline(false);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;
    assert_eq!(engine.mail_window(&[account()], 50).await.unwrap().len(), 5);
}

#[tokio::test]
async fn live_push_surfaces_new_mail_immediately() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(3), 3);
    let view = Mutex::new(ClientView::default());
    let observer = |commit: &SyncCommit<'_>| view.lock().unwrap().apply(commit);

    // Initial sync fills the view.
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &observer,
        )
        .await;
    assert_eq!(view.lock().unwrap().len(), 3);

    // A watcher fires (new mail arrived). The host runs the scope's normal sync; the
    // delta commits the new message and the change event carries it, so the client
    // splices it in with no whole-list re-query.
    provider.deliver(message("m-new", "Fresh mail", "2026-06-16T09:00:00Z"));
    let report = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &observer,
        )
        .await;
    assert_eq!(report.upserted(), 1);
    assert_eq!(
        view.lock().unwrap().len(),
        4,
        "the new message appeared immediately"
    );
    assert_eq!(
        view.lock().unwrap().subjects.get(&key("m-new")),
        Some(&"Fresh mail".to_owned())
    );
}

#[tokio::test]
async fn startup_loads_the_initial_page_well_under_500ms() {
    // A large cached mailbox (the perf-sensitive warm start). Seed it, then time the
    // initial-page read a host does on launch.
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5_000), 500);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            StreamTuning::bulk(),
            &no_observer(),
        )
        .await;

    let started = Instant::now();
    let page = engine.mail_window(&[account()], 50).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(page.len(), 50, "the initial page of 50 loaded");
    assert!(
        elapsed.as_millis() < 500,
        "initial page load took {elapsed:?}, over the 500ms startup budget"
    );
}

#[tokio::test]
async fn missing_body_work_list_shrinks_as_a_warm_pass_fetches() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5), 5);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;

    // A metadata-only sync leaves every body unwarmed — the work list is the whole
    // window, newest first (m0), same ranking as the windowed read.
    let missing = engine.mail_missing_body(&[account()], 50).await.unwrap();
    assert_eq!(missing.len(), 5);
    assert_eq!(missing[0].mail.subject.as_deref(), Some("Subject 0"));

    // Warm the two newest — the work list drops exactly those and keeps ranking.
    warm(&engine, &provider, &missing[..2]).await;
    let rest = engine.mail_missing_body(&[account()], 50).await.unwrap();
    assert_eq!(rest.len(), 3);
    assert_eq!(rest[0].mail.subject.as_deref(), Some("Subject 2"));

    // The cap keeps the newest *missing*, not just the newest.
    let capped = engine.mail_missing_body(&[account()], 1).await.unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].mail.subject.as_deref(), Some("Subject 2"));

    // A fully-warm window returns an empty work list.
    warm(&engine, &provider, &rest).await;
    assert!(
        engine
            .mail_missing_body(&[account()], 50)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Fetches each row's body, resolving the whole message the fetch needs from its key — the shape
/// a host's warming pass has, now that the work list is rows rather than objects.
async fn warm(engine: &Engine, provider: &SimProvider, rows: &[engine_api::MailListRow]) {
    for row in rows {
        let message = engine
            .messages_by_keys(&account(), core::slice::from_ref(&row.mail.key))
            .await
            .unwrap();
        engine
            .message_body(provider, &account(), &message[0])
            .await
            .unwrap();
    }
}

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

/// A no-op observer for syncs whose progress a test does not inspect.
fn no_observer() -> impl engine_api::SyncObserver {
    engine_api::IgnoreCommits
}

#[tokio::test]
async fn warming_a_body_fills_the_list_snippet_a_provider_never_sent() {
    // IMAP sends no server snippet, so a synced row has an empty preview and the list shows
    // sender and subject with nothing under them. The snippet has to be derived from the body,
    // and the body warming pass is the one place every message's body passes through —
    // a backfill's rows, a delta's arrival and an on-demand open all end up here.
    //
    // This is the bug the first real cold sync surfaced: 5,022 of 5,022 IMAP rows had no
    // preview, while every provider-supplied one did.
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(3), 3);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;

    let before = engine.mail_window(&[account()], 10).await.unwrap();
    assert_eq!(before.len(), 3);
    assert!(
        before.iter().all(|row| row.mail.preview.is_none()),
        "the fake sends no snippet, so the sync cannot have invented one"
    );

    warm(&engine, &provider, &before).await;

    let after = engine.mail_window(&[account()], 10).await.unwrap();
    assert!(
        after
            .iter()
            .all(|row| row.mail.preview.as_deref() == Some("warmed body")),
        "every warmed row carries the snippet its body yielded: {:?}",
        after
            .iter()
            .map(|r| r.mail.preview.clone())
            .collect::<Vec<_>>()
    );
}
