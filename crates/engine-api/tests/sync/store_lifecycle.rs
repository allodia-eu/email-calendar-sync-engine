//! The store's own lifecycle through the facade: resetting cursors, forgetting an account,
//! pruning out-of-window mail, and vacuuming — the operations that reshape what is stored
//! rather than sync it.
//!
//! Split from `sync_lifecycle.rs` to keep both inside the 500-line limit.

use engine_api::{Engine, TimeZoneId};

use super::*;

#[tokio::test]
async fn reset_clears_cursors_and_forces_a_full_resync() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    // First sync is a snapshot (2 upserts); a second is an empty delta off the cursor.
    let first = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(first.upserted(), 2);
    let delta = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(delta.upserted(), 0);

    // Reset clears the cursors, so the next sync re-snapshots (full refetch) again.
    engine.reset().await.unwrap();
    let resynced = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(resynced.upserted(), 2);
}

#[tokio::test]
async fn forget_account_purges_the_account_and_a_re_add_starts_clean() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    // Sync the account: mail (2) and calendar (1) land, and the scopes carry cursors —
    // a second mail sync is an empty delta off the persisted cursor.
    engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
    let redelta = engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(redelta.upserted(), 0, "cursor persisted before forget");

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
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(readd.upserted(), 2, "re-add re-snapshots from scratch");
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
}

#[tokio::test]
async fn prune_drops_out_of_window_mail_offline() {
    let engine = Engine::open_in_memory().unwrap();
    // Mail synced under a wide window: one old message and one recent one land locally.
    let provider = FakeProvider {
        messages: vec![
            dated_message("old", "old@h", &[], "2026-01-15T09:00:00Z"),
            dated_message("recent", "recent@h", &[], "2026-06-20T09:00:00Z"),
        ],
        ..FakeProvider::new()
    };
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Narrowing to an unbounded window removes nothing — nothing is "outside" it.
    let full = engine
        .prune_account_mail_outside_window(&account(), SyncWindow::full())
        .await
        .unwrap();
    assert_eq!(full.messages_removed, 0);
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Narrowing depth to a 2026-04-01 floor prunes the January message locally, with no
    // provider round trip — the offline equivalent of a narrower re-snapshot.
    let floor = engine_core::time::CalendarDate::new(2026, 4, 1).unwrap();
    let report = engine
        .prune_account_mail_outside_window(&account(), SyncWindow::since(floor))
        .await
        .unwrap();
    assert_eq!(report.messages_removed, 1);

    // Only the in-window message remains, and it reads back intact.
    let remaining = engine.messages(&account()).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id.key().as_str(), "recent");
}

#[tokio::test]
async fn vacuum_compacts_the_store_without_losing_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();

    engine
        .sync_mail(
            core::slice::from_ref(&FakeProvider::new()),
            &account(),
            plain(),
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // Compaction runs without error and keeps the live rows readable — the store-sqlite
    // test proves it reclaims the freed pages and shrinks the file on disk.
    engine.vacuum().await.unwrap();
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
}
