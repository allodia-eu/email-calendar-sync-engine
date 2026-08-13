//! What the reader pool guarantees, and what it deliberately does not.
//!
//! Splitting reads onto their own connections changes when a read sees a write, so the two
//! questions that decides are pinned here rather than left to WAL folklore:
//!
//! 1. **Read-your-writes holds.** Once an apply has returned, every later read sees it — there is
//!    no window where a committed change is invisible. Anything the engine does as `write.await;
//!    read.await` is safe by construction.
//! 2. **A read taken *during* an uncommitted write sees the state before it, and does not wait.**
//!    That is the point of the split, and it is the behaviour change: under one mutex such a read
//!    could only run before or after the write, never beside it.
//!
//! Neither is a race in the engine's own paths — sync applies and then reports, and the observer
//! runs after the commit — but a caller that *races* a read against a write it did not await now
//! gets the pre-write snapshot instead of an arbitrary ordering. That is the safer of the two: a
//! snapshot is always internally consistent, where an interleaved read never was.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};
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

fn scope_of(account: &AccountId) -> SyncScope {
    SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Email,
    }
}

/// Writes one object through the fenced apply path and returns the store it wrote to.
async fn store_with_one_object(path: &std::path::Path, data: &str) -> SqliteStore<ManualClock> {
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
    let store = SqliteStore::open(path, clock).expect("open file store");
    let account = AccountId::try_from("acct").expect("valid account");
    let claim = store
        .claim_sync_scope(
            account.clone(),
            &scope_of(&account),
            LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5)),
        )
        .await
        .expect("claim");
    let update = SyncUpdate::delta(
        vec![TestObject {
            key: ProviderKey::new("m1").expect("valid key"),
            data: data.to_owned(),
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
    store
}

/// The stored `data` field for `m1`, read through the pool.
async fn read_data(store: &SqliteStore<ManualClock>, account: &AccountId) -> Option<String> {
    store
        .object_payload(&scope_of(account), &ProviderKey::new("m1").expect("key"))
        .await
        .expect("read")
        .map(|payload| payload["data"].as_str().expect("a data field").to_owned())
}

#[tokio::test]
async fn a_committed_apply_is_visible_to_every_reader_at_once() {
    // Read-your-writes across connections. The readers are opened at construction, *before* this
    // write existed, so this also covers the case that worried nobody until it did: a connection
    // that predates a commit still sees it, because a WAL reader takes its snapshot per statement
    // rather than at open.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let store = store_with_one_object(&path, "first").await;
    let account = AccountId::try_from("acct").expect("valid account");

    assert_eq!(read_data(&store, &account).await.as_deref(), Some("first"));

    // Enough reads to touch every connection in the pool, so a stale one cannot hide behind a
    // fresh one — the reader is picked round-robin, and one read would only prove one connection.
    let claim = store
        .claim_sync_scope(
            account.clone(),
            &scope_of(&account),
            LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5)),
        )
        .await
        .expect("claim");
    let update = SyncUpdate::delta(
        vec![TestObject {
            key: ProviderKey::new("m1").expect("valid key"),
            data: "second".to_owned(),
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
                &SyncState::new("cursor-2"),
            ),
        )
        .await
        .expect("apply");
    store
        .release_sync_scope(claim.lease)
        .await
        .expect("release");

    for attempt in 0..16 {
        assert_eq!(
            read_data(&store, &account).await.as_deref(),
            Some("second"),
            "read {attempt} saw a stale snapshot after the apply returned"
        );
    }
}

#[tokio::test]
async fn a_read_beside_an_uncommitted_write_sees_the_state_before_it_and_does_not_wait() {
    // The behaviour the split introduces, held still: a *separate* connection opens a write
    // transaction and does not commit, standing in for a sync mid-apply. Under one mutex this read
    // could not have run at all until the write finished.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    let store = store_with_one_object(&path, "before").await;
    let account = AccountId::try_from("acct").expect("valid account");

    let mut writer = rusqlite::Connection::open(&path).expect("raw open");
    writer
        .execute_batch("PRAGMA busy_timeout = 5000;")
        .expect("pragma");
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("begin immediate");
    tx.execute(
        "UPDATE object SET payload = json_set(payload, '$.data', 'after')",
        [],
    )
    .expect("update");

    // The write lock is held and nothing is committed. The read must still answer, with the
    // pre-write value. A `busy_timeout` of five seconds means a blocked read would take five
    // seconds and then fail rather than hang, so this cannot deadlock the suite either way.
    assert_eq!(
        read_data(&store, &account).await.as_deref(),
        Some("before"),
        "an uncommitted write must not be visible, and must not block the read"
    );

    tx.commit().expect("commit");

    assert_eq!(
        read_data(&store, &account).await.as_deref(),
        Some("after"),
        "the commit is visible to the pool as soon as it returns"
    );
}
