//! Sync mechanics and store lifecycle: mail/calendar snapshot-then-delta, cursor
//! persistence across reopen, delta tombstoning, provider-failure surfacing, the
//! same-scope busy race, streaming sync, thread derivation, and reset/vacuum.

use engine_api::{ApiError, Engine, SyncProgress, TimeZoneId};
use engine_core::ids::ThreadId;

use super::*;

#[tokio::test]
async fn syncs_mail_from_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let report = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    // First sync is a snapshot: both containers and both members are upserted.
    assert_eq!(report.mailboxes.upserted, 2);
    assert_eq!(report.email.upserted, 2);
    assert_eq!(report.email.tombstoned, 0);
}

#[tokio::test]
async fn syncs_calendar_from_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let report = engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(report.calendars.upserted, 1);
    assert_eq!(report.events.upserted, 1);
}

#[tokio::test]
async fn reopen_resumes_mail_from_the_persisted_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");

    let first = Engine::open(&db).unwrap();
    let initial = first
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(initial.email.upserted, 2); // first sync is a snapshot
    drop(first);

    // Reopen and sync again. Because the cursor persisted, the fake is asked for a
    // *delta* and returns an empty one — so nothing is upserted. On a fresh/lost DB
    // there would be no cursor, the fake would return a snapshot, and upserted would
    // be 2. Asserting 0 is therefore a real persistence check, not a re-apply count.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(resumed.email.upserted, 0);
    assert_eq!(resumed.email.tombstoned, 0);
}

#[tokio::test]
async fn reopen_resumes_calendar_from_the_persisted_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let first = Engine::open(&db).unwrap();
    let initial = first
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(initial.events.upserted, 1);
    drop(first);

    // Same persistence check for the on-disk calendar/event/occurrence path: the
    // resumed sync is an empty delta off the persisted cursor.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(resumed.events.upserted, 0);
}

#[tokio::test]
async fn resync_tombstones_mail_dropped_from_the_delta() {
    let engine = Engine::open_in_memory().unwrap();
    // m1's stored key is its MessageId's provider key — recompute it from the same id.
    let dropped = message("m1", "a", "Quarterly report").id.key().clone();
    let provider = FakeProvider::new().removing_on_resync(vec![dropped]);

    let initial = engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(initial.email.upserted, 2);

    // The cursor now exists, so the second sync is a delta that drops m1: it must be
    // tombstoned, with nothing upserted.
    let resync = engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(resync.email.tombstoned, 1);
    assert_eq!(resync.email.upserted, 0);
}

#[tokio::test]
async fn mail_provider_failure_surfaces_as_a_sync_error() {
    let engine = Engine::open_in_memory().unwrap();
    let err = engine
        .sync_mail(&FakeProvider::failing(), &account())
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn calendar_provider_failure_surfaces_as_a_sync_error() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let err = engine
        .sync_calendar(&FakeProvider::failing(), &account(), horizon(), &zone)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn concurrent_same_scope_sync_reports_busy() {
    let engine = Engine::open_in_memory().unwrap();
    let acct = account();
    let (claim_tx, claim_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = GateProvider {
        inner: FakeProvider::new(),
        on_claim: std::sync::Mutex::new(Some(claim_tx)),
        until_release: std::sync::Mutex::new(Some(release_rx)),
    };

    // The gated sync claims the mailbox scope and parks (lease held) until released.
    let held = engine.sync_mail(&gate, &acct);
    // The racer waits until the lease is held, then attempts the same scope.
    let racer = async {
        claim_rx.await.expect("first sync should claim the scope");
        let outcome = engine.sync_mail(&FakeProvider::new(), &acct).await;
        release_tx.send(()).expect("first sync still parked");
        outcome
    };

    let (held_result, racer_result) = tokio::join!(held, racer);
    held_result.expect("the lease holder completes once released");

    // The racer found the scope's lease live -> retryable ScopeHeld -> ApiError::Busy,
    // not an opaque sync error.
    let err = racer_result.expect_err("the racer must lose the scope race");
    assert!(matches!(err, ApiError::Busy), "got {err:?}");
    assert_eq!(
        err.to_string(),
        "scope is busy: another sync is in progress; retry shortly"
    );
}

#[tokio::test]
async fn open_rejects_an_unusable_path() {
    let dir = tempfile::tempdir().unwrap();
    // A database file under a directory that does not exist cannot be created.
    let bad = dir.path().join("missing").join("engine.sqlite");
    let err = Engine::open(&bad).unwrap_err();
    assert!(matches!(err, ApiError::Store(_)), "got {err:?}");
}

#[tokio::test]
async fn clear_mail_cursors_forces_a_reconciling_resnapshot() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let engine = Engine::open_in_memory().unwrap();
    let dropped = Arc::new(AtomicBool::new(false));
    let provider = ReconcilingProvider {
        caps: Capabilities::none().with_mail(),
        dropped: Arc::clone(&dropped),
    };

    // First sync snapshots both messages.
    engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // The server drops m2 (a move/expunge). A plain re-sync is a delta carrying no
    // removals (IMAP without CONDSTORE), so it does NOT reconcile — m2 lingers.
    dropped.store(true, Ordering::SeqCst);
    engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Clearing the mail cursors forces the next sync to snapshot, which tombstones the
    // now-absent m2 — the reconcile a host triggers for a "refresh" that reflects
    // server-side changes.
    engine.clear_mail_cursors(&account()).await.unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn sync_mail_streamed_reports_progress() {
    use std::sync::{Arc, Mutex};
    let engine = Engine::open_in_memory().unwrap();
    let seen: Arc<Mutex<Vec<SyncProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        // The blanket `ProgressSink for Fn(SyncProgress)` impl lets a closure be the sink.
        move |p: SyncProgress| seen.lock().unwrap().push(p)
    };

    let report = engine
        .sync_mail_streamed(&FakeProvider::new(), &account(), 0, &sink)
        .await
        .unwrap();
    assert_eq!(report.email.upserted, 2);

    // The fake returns both messages in one snapshot page whose total is known up
    // front, so exactly one progress event lands with fetched == total == 2.
    let progress = seen.lock().unwrap();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].fetched, 2);
    assert_eq!(progress[0].total, Some(2));
}

#[tokio::test]
async fn folder_split_sync_lists_then_streams_email() {
    use std::sync::{Arc, Mutex};
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeProvider::new();

    // The container step applies only the folder list — the messages are not synced yet,
    // so the per-folder email streams can fan out afterwards without re-listing.
    let mailboxes = engine
        .sync_mailbox_list(&provider, &account())
        .await
        .unwrap();
    assert_eq!(mailboxes.upserted, 2);
    assert_eq!(engine.mailboxes(&account()).await.unwrap().len(), 2);
    assert!(engine.messages(&account()).await.unwrap().is_empty());

    // The per-folder email stream then commits the messages and reports progress,
    // without re-touching the folder list.
    let seen: Arc<Mutex<Vec<SyncProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        move |p: SyncProgress| seen.lock().unwrap().push(p)
    };
    let email = engine
        .sync_folder_email_streamed(&provider, &account(), 0, &sink)
        .await
        .unwrap();
    assert_eq!(email.upserted, 2);
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn derives_and_persists_thread_ids_for_unthreaded_mail() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::threaded(), &account())
        .await
        .unwrap();

    // IMAP-shaped mail arrives without thread ids.
    let before = engine.messages(&account()).await.unwrap();
    assert!(before.iter().all(|m| m.thread_id.is_none()));

    // Derivation groups the reply (t2) with its original (t1); t3 stands alone.
    let report = engine.derive_mail_threads(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 3);
    assert_eq!(report.threads, 2);

    // The grouping is persisted: messages() now carries the derived thread_id.
    let after = engine.messages(&account()).await.unwrap();
    let thread_of = |key: &str| {
        after
            .iter()
            .find(|m| m.id.key().as_str() == key)
            .unwrap()
            .thread_id
            .clone()
    };
    assert!(thread_of("t1").is_some());
    assert_eq!(thread_of("t1"), thread_of("t2"));
    assert_ne!(thread_of("t1"), thread_of("t3"));
}

#[tokio::test]
async fn derive_mail_threads_is_a_noop_for_provider_threaded_mail() {
    // A provider that assigns its own thread ids (JMAP/Gmail/Graph): derivation must
    // not touch them.
    let mut provider = FakeProvider::threaded();
    for (index, message) in provider.messages.iter_mut().enumerate() {
        message.thread_id = Some(ThreadId::try_from(format!("T{index}").as_str()).unwrap());
    }
    let engine = Engine::open_in_memory().unwrap();
    engine.sync_mail(&provider, &account()).await.unwrap();

    let report = engine.derive_mail_threads(&account()).await.unwrap();
    assert_eq!(report.messages_assigned, 0);

    // Every message keeps its provider-assigned thread id.
    let after = engine.messages(&account()).await.unwrap();
    assert_eq!(after.len(), 3);
    assert!(after.iter().all(|m| m.thread_id.is_some()));
}

#[tokio::test]
async fn reset_clears_cursors_and_forces_a_full_resync() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    // First sync is a snapshot (2 upserts); a second is an empty delta off the cursor.
    let first = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(first.email.upserted, 2);
    let delta = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(delta.email.upserted, 0);

    // Reset clears the cursors, so the next sync re-snapshots (full refetch) again.
    engine.reset().await.unwrap();
    let resynced = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(resynced.email.upserted, 2);
}

#[tokio::test]
async fn forget_account_purges_the_account_and_a_re_add_starts_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    // Sync the account: mail (2) and calendar (1) land, and the scopes carry cursors —
    // a second mail sync is an empty delta off the persisted cursor.
    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
    let redelta = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(redelta.email.upserted, 0, "cursor persisted before forget");

    // Forgetting the account drops its objects and scopes: reads are empty, and search
    // (which ranks over the derived rows) finds nothing.
    engine.forget_account(&account()).await.unwrap();
    assert!(engine.messages(&account()).await.unwrap().is_empty());
    assert!(engine.mailboxes(&account()).await.unwrap().is_empty());
    assert!(
        engine
            .search_mail(&account(), "report", 10)
            .await
            .unwrap()
            .hits
            .is_empty()
    );

    // Re-adding the same account starts clean: the scopes were forgotten, so the next
    // sync is a full snapshot again (upserted == 2), not an empty delta off a stale
    // cursor. That is the remove-then-re-add guarantee.
    let readd = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(readd.email.upserted, 2, "re-add re-snapshots from scratch");
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
}

#[tokio::test]
async fn vacuum_compacts_the_store_without_losing_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Compaction runs without error and keeps the live rows readable — the store-sqlite
    // test proves it reclaims the freed pages and shrinks the file on disk.
    engine.vacuum().await.unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
}
