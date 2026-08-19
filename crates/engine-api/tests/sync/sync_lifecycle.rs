//! Sync mechanics: mail/calendar snapshot-then-delta, cursor persistence across reopen, delta
//! tombstoning, provider-failure surfacing, the same-scope busy race, and the account pass.
//! Thread derivation lives in its own `threading` module, and the operations that reshape the
//! store rather than sync it in `store_lifecycle`.

use engine_api::{ApiError, Engine, StreamTuning, SyncCommit, TimeZoneId};

use super::*;

#[tokio::test]
async fn syncs_mail_from_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let report = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    // First sync is a snapshot: both containers and both members are upserted.
    assert_eq!(report.mailboxes.as_ref().unwrap().upserted, 2);
    assert_eq!(report.upserted(), 2);
    assert_eq!(report.tombstoned(), 0);
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
    assert_eq!(report.events.applied.upserted, 1);
}

#[tokio::test]
async fn reopen_resumes_mail_from_the_persisted_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");

    let first = Engine::open(&db).unwrap();
    let initial = first
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(initial.upserted(), 2); // first sync is a snapshot
    drop(first);

    // Reopen and sync again. Because the cursor persisted, the fake is asked for a
    // *delta* and returns an empty one — so nothing is upserted. On a fresh/lost DB
    // there would be no cursor, the fake would return a snapshot, and upserted would
    // be 2. Asserting 0 is therefore a real persistence check, not a re-apply count.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(resumed.upserted(), 0);
    assert_eq!(resumed.tombstoned(), 0);
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
    assert_eq!(initial.events.applied.upserted, 1);
    drop(first);

    // Same persistence check for the on-disk calendar/event/occurrence path: the
    // resumed sync is an empty delta off the persisted cursor.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(resumed.events.applied.upserted, 0);
}

#[tokio::test]
async fn resync_tombstones_mail_dropped_from_the_delta() {
    let engine = Engine::open_in_memory().unwrap();
    // m1's stored key is its MessageId's provider key — recompute it from the same id.
    let dropped = message("m1", "a", "Quarterly report").id.key().clone();
    let provider = FakeProvider::new().removing_on_resync(vec![dropped]);

    let initial = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(initial.upserted(), 2);

    // The cursor now exists, so the second sync is a delta that drops m1: it must be
    // tombstoned, with nothing upserted.
    let resync = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(resync.tombstoned(), 1);
    assert_eq!(resync.upserted(), 0);
}

#[tokio::test]
async fn mail_provider_failure_surfaces_as_a_sync_error() {
    let engine = Engine::open_in_memory().unwrap();
    let report = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::failing()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    // The folder list is the first thing a provider is asked for, so a provider that fails
    // everything fails there — and the report says *that*, rather than collapsing the pass into
    // one error that cannot say which scope broke.
    assert!(report.mailboxes.is_err(), "got {:?}", report.mailboxes);
    assert!(!report.is_ok());
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
    let quiet = quiet();
    let held = engine.sync_mail(core::slice::from_ref(&gate), &acct, plain(), &quiet);
    // The racer waits until the lease is held, then attempts the same scope.
    let racer = async {
        claim_rx.await.expect("first sync should claim the scope");
        let outcome = engine
            .sync_mail(
                core::slice::from_ref(&FakeProvider::new()),
                &acct,
                plain(),
                &quiet,
            )
            .await;
        release_tx.send(()).expect("first sync still parked");
        outcome
    };

    let (held_result, racer_result) = tokio::join!(held, racer);
    assert!(
        held_result.is_ok(),
        "the lease holder completes once released"
    );

    // The racer found the folder-list lease live, so that scope reports ScopeHeld — and the
    // report says which scope, rather than collapsing the pass into one error. Being busy is not
    // a failure of the account: nothing was asked of the server.
    assert_eq!(racer_result.busy_scopes(), 1);
    let err = racer_result
        .mailboxes
        .as_ref()
        .expect_err("the racer must lose the scope race");
    assert!(err.is_busy(), "got {err:?}");
}

#[tokio::test]
async fn abandon_sync_leases_recovers_an_aborted_sync_without_losing_cursor() {
    use std::sync::Arc;

    let engine = Arc::new(Engine::open_in_memory().unwrap());
    let acct = account();
    let initial = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &acct,
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(initial.upserted(), 2);

    let (claim_tx, claim_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = Arc::new(GateProvider {
        inner: FakeProvider::new(),
        on_claim: std::sync::Mutex::new(Some(claim_tx)),
        until_release: std::sync::Mutex::new(Some(release_rx)),
    });

    let held_engine = Arc::clone(&engine);
    let held_gate = Arc::clone(&gate);
    let held_acct = acct.clone();
    let held = tokio::spawn(async move {
        held_engine
            .sync_mail(
                core::slice::from_ref(&*held_gate),
                &held_acct,
                StreamTuning::new(0, 0),
                &engine_api::IgnoreCommits,
            )
            .await
    });
    claim_rx.await.expect("sync should claim a scope");
    held.abort();
    assert!(held.await.unwrap_err().is_cancelled());
    drop(release_tx);

    assert_eq!(engine.abandon_sync_leases().await.unwrap(), 1);
    let resumed = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &acct,
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(resumed.mailboxes.as_ref().unwrap().upserted, 0);
    assert_eq!(resumed.upserted(), 0);
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
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // The server drops m2 (a move/expunge). A plain re-sync is a delta carrying no
    // removals (IMAP without CONDSTORE), so it does NOT reconcile — m2 lingers.
    dropped.store(true, Ordering::SeqCst);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Clearing the mail cursors forces the next sync to snapshot, which tombstones the
    // now-absent m2 — the reconcile a host triggers for a "refresh" that reflects
    // server-side changes.
    engine.clear_mail_cursors(&account()).await.unwrap();
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_pass_reports_streaming_progress() {
    use std::sync::{Arc, Mutex};
    // The (fetched, total) each commit reported, factored out so the shared handle's
    // type stays simple (clippy::type_complexity).
    type Reported = (usize, Option<usize>);
    let engine = Engine::open_in_memory().unwrap();
    let seen: Arc<Mutex<Vec<Reported>>> = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let seen = Arc::clone(&seen);
        // The blanket `SyncObserver for Fn(&SyncCommit)` impl lets a closure be the observer.
        move |c: &SyncCommit<'_>| seen.lock().unwrap().push((c.fetched, c.total))
    };

    let report = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            StreamTuning::new(0, 0),
            &observer,
        )
        .await;
    assert_eq!(report.upserted(), 2);

    // The fake returns both messages in one snapshot chunk whose total is known up
    // front, so exactly one commit lands with fetched == total == 2.
    let progress = seen.lock().unwrap();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].0, 2);
    assert_eq!(progress[0].1, Some(2));
}

#[tokio::test]
async fn an_account_pass_syncs_the_folder_list_and_then_its_mail() {
    use std::sync::{Arc, Mutex};
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeProvider::new();

    // One call does both halves, and the report keeps them apart: the folder-list scope is a
    // container the engine syncs once before fanning the folders out, and a caller that needs to
    // know which half failed can still see it.
    let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let seen = Arc::clone(&seen);
        move |c: &SyncCommit<'_>| seen.lock().unwrap().push(c.fetched)
    };
    let report = engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            StreamTuning::new(0, 0),
            &observer,
        )
        .await;

    assert!(report.is_ok());
    assert_eq!(report.mailboxes.as_ref().unwrap().upserted, 2);
    assert_eq!(report.upserted(), 2);
    assert_eq!(report.folders_synced(), 1);
    assert_eq!(engine.mailboxes(&account()).await.unwrap().len(), 2);
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
    assert_eq!(seen.lock().unwrap().len(), 1);
}
