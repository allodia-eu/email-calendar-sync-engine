//! File-backed durability: committed state survives a reopen, and file databases
//! run in WAL mode. The shared contract suite runs on `:memory:`, so this is the
//! one place the on-disk path (persistence + the WAL pragma) is exercised.

use core::time::Duration;

use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_store::{
    ApplyBatch, DerivedWrite, LeaseRequest, ManualClock, StorableObject, Store, StoreRead, WorkerId,
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

#[tokio::test]
async fn committed_state_survives_reopen_and_file_db_uses_wal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
    let account = AccountId::try_from("acct").expect("valid account");
    let scope = SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Email,
    };

    // First session: write one object and a cursor, then release and drop.
    {
        let store = SqliteStore::open(&path, clock.clone()).expect("open file store");
        let claim = store
            .claim_sync_scope(
                account.clone(),
                &scope,
                LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5)),
            )
            .await
            .expect("claim");
        let update = SyncUpdate::delta(
            vec![TestObject {
                key: ProviderKey::new("m1").unwrap(),
                data: "hello".to_owned(),
            }],
            vec![],
        );
        store
            .apply_sync_update(
                &claim.lease,
                ApplyBatch::new(
                    &update,
                    &DerivedWrite::empty(),
                    &[],
                    &SyncState::new("cursor-1"),
                ),
            )
            .await
            .expect("apply");
        store
            .release_sync_scope(claim.lease)
            .await
            .expect("release");
    }

    // Second session: the committed object and cursor are still there.
    let store = SqliteStore::open(&path, clock).expect("reopen file store");
    assert_eq!(
        store.object_keys(&scope).await.unwrap(),
        vec![ProviderKey::new("m1").unwrap()]
    );
    assert_eq!(
        store.load_sync_state(account, &scope).await.unwrap(),
        Some(SyncState::new("cursor-1"))
    );

    // A raw connection confirms the database is in WAL mode.
    let raw = rusqlite::Connection::open(&path).expect("raw open");
    let mode: String = raw
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode, "wal");
}
