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
async fn a_delta_cannot_re_admit_mail_from_outside_the_window() {
    // "New arrival" means new to us, not recent. IMAP has no in-place edit, so filing an old
    // message into a folder mints a UID above the cursor and the delta reports it as an
    // arrival; Graph, Gmail and JMAP deltas do the same for a move. Unfiltered, that walks
    // mail back past a depth the user chose — and nothing takes it out again, because a delta
    // never re-lists what it did not change.
    let engine = Engine::open_in_memory().unwrap();
    let floor = engine_core::time::CalendarDate::new(2026, 4, 1).unwrap();
    let windowed = plain().within(SyncWindow::since(floor));
    let provider = FakeProvider {
        messages: vec![dated_message(
            "recent",
            "recent@h",
            &[],
            "2026-06-20T09:00:00Z",
        )],
        ..FakeProvider::new()
    }
    .adding_on_resync(vec![dated_message(
        "filed",
        "filed@h",
        &[],
        "2023-02-01T09:00:00Z",
    )]);

    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            windowed,
            &quiet(),
        )
        .await;
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 1);

    // The cursored resync's delta carries the old message. It must not land.
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            windowed,
            &quiet(),
        )
        .await;

    let keys: Vec<String> = engine
        .messages(&account())
        .await
        .unwrap()
        .into_iter()
        .map(|message| message.id.key().as_str().to_owned())
        .collect();
    assert_eq!(keys, vec!["recent"], "an out-of-window arrival was stored");
}

#[tokio::test]
async fn a_snapshot_is_the_adapters_call_and_is_not_second_guessed_here() {
    // The counterpart of the test above, and the reason the filter is delta-only. A
    // reconcile carries a present set, and the store tombstones by diffing against it, so
    // the adapter has already decided what the window holds — using its server's date
    // semantics, not ours. IMAP `SINCE` compares `INTERNALDATE` in the *server's* timezone,
    // so re-deciding here would drop a message the snapshot deliberately asked for while its
    // key stayed in `present`: absent from the store, and never tombstoned either.
    let engine = Engine::open_in_memory().unwrap();
    let floor = engine_core::time::CalendarDate::new(2026, 4, 1).unwrap();
    let provider = FakeProvider {
        messages: vec![dated_message("old", "old@h", &[], "2023-02-01T09:00:00Z")],
        ..FakeProvider::new()
    };

    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            plain().within(SyncWindow::since(floor)),
            &quiet(),
        )
        .await;

    assert_eq!(
        engine.messages(&account()).await.unwrap().len(),
        1,
        "a snapshot's own enumeration was overruled"
    );
}

#[tokio::test]
async fn an_unbounded_window_stores_every_arrival_however_old() {
    // The filter is the window's, not a freshness rule of its own: with no floor there is
    // nothing outside it, and an account synced over all time keeps what it is sent.
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeProvider {
        messages: vec![dated_message(
            "recent",
            "recent@h",
            &[],
            "2026-06-20T09:00:00Z",
        )],
        ..FakeProvider::new()
    }
    .adding_on_resync(vec![dated_message(
        "filed",
        "filed@h",
        &[],
        "2023-02-01T09:00:00Z",
    )]);

    for _ in 0..2 {
        engine
            .sync_mail(
                core::slice::from_ref(&provider),
                &account(),
                plain(),
                &quiet(),
            )
            .await;
    }

    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);
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
