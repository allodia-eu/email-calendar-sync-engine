//! VACUUM reclaims the free pages deletions leave behind, so the on-disk file shrinks back
//! toward the live data's size instead of staying at its high-water mark. The shared
//! contract suite runs on `:memory:` (no file to shrink), so this is the one place the
//! compaction path is exercised against a real file — the engine-side of "the database
//! stays at ~700 MB after a reset" (`store.vacuum`, run by the host after a reset re-sync).

use core::time::Duration;

use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_store::{
    ApplyBatch, DerivedWrite, LeaseRequest, ManualClock, StorableObject, Store, WorkerId,
};
use serde::{Deserialize, Serialize};
use store_sqlite::SqliteStore;

#[derive(Serialize, Deserialize)]
struct TestObject {
    key: ProviderKey,
    data: String,
}

impl StorableObject for TestObject {
    fn provider_key(&self) -> &ProviderKey {
        &self.key
    }
}

/// The database's logical page count, free-page count, and on-disk file size. Read with no
/// store connection open, so the WAL is already folded into the main file (SQLite
/// checkpoints when the last connection closes) — every figure is deterministic.
fn db_stats(path: &std::path::Path) -> (i64, i64, u64) {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("page_count");
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .expect("freelist_count");
    drop(conn);
    let size = std::fs::metadata(path).expect("metadata").len();
    (page_count, freelist, size)
}

/// Applies `update` under a fresh lease and a new cursor, then releases — one self-contained
/// write "session" against the email scope.
async fn write_session(
    store: &SqliteStore<ManualClock>,
    account: &AccountId,
    scope: &SyncScope,
    update: &SyncUpdate<TestObject>,
    cursor: &str,
) {
    let claim = store
        .claim_sync_scope(
            account.clone(),
            scope,
            LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5)),
        )
        .await
        .expect("claim");
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(update, &DerivedWrite::empty(), &[], &SyncState::new(cursor)),
        )
        .await
        .expect("apply");
    store
        .release_sync_scope(claim.lease)
        .await
        .expect("release");
}

#[tokio::test]
async fn vacuum_reclaims_freed_pages_and_shrinks_the_file() {
    // Enough ~1 KiB objects that the freed space dwarfs the empty-schema baseline.
    const N: usize = 1000;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
    let account = AccountId::try_from("acct").expect("valid account");
    let scope = SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Email,
    };

    let keys: Vec<ProviderKey> = (0..N)
        .map(|i| ProviderKey::new(format!("m{i}")).expect("non-empty key"))
        .collect();

    let store = SqliteStore::open(&path, clock.clone()).expect("open file store");

    // Insert N objects, then delete every one — the high-water mark the symptom describes.
    let inserts = SyncUpdate::delta(
        keys.iter()
            .map(|key| TestObject {
                key: key.clone(),
                data: "x".repeat(1024),
            })
            .collect(),
        vec![],
    );
    write_session(&store, &account, &scope, &inserts, "c1").await;
    let deletes = SyncUpdate::<TestObject>::delta(vec![], keys.clone());
    write_session(&store, &account, &scope, &deletes, "c2").await;

    // Close so the WAL folds into the file before measuring.
    drop(store);
    let (pages_before, free_before, size_before) = db_stats(&path);
    assert!(
        free_before > 0,
        "deletion frees pages but SQLite retains them at the high-water mark \
         (freelist={free_before})"
    );

    // Re-open and compact.
    let store = SqliteStore::open(&path, clock).expect("reopen file store");
    store.vacuum().await.expect("vacuum");
    drop(store);

    let (pages_after, free_after, size_after) = db_stats(&path);
    assert_eq!(free_after, 0, "VACUUM reclaims every free page");
    assert!(
        pages_after < pages_before,
        "VACUUM drops the page count ({pages_after} < {pages_before})"
    );
    assert!(
        size_after < size_before,
        "the on-disk file shrinks ({size_after} < {size_before} bytes)"
    );
}
