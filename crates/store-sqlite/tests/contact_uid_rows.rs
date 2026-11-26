//! A contact card stored when its `uid` was a plain string still loads.
//!
//! `ContactCard::uid` became a `Uid`, which takes neither an empty string nor one over 1024
//! octets, and a stored row may hold either: an earlier build kept whatever the source sent, and
//! Google's resource names and Graph's `urn:microsoft:graph:` ids beside them. The people index
//! reads every stored card in one pass, so one row that failed to load would fail them all.

use core::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactName},
    ids::{AccountId, AddressBookId, ContactId, Uid},
    membership::Memberships,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};
use engine_store::{
    ApplyBatch, ContactStore, DerivedWrite, LeaseRequest, ManualClock, Store, WorkerId,
};
use store_sqlite::SqliteStore;

fn open(path: &std::path::Path) -> SqliteStore<ManualClock> {
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
    SqliteStore::open(path, clock).expect("open file store")
}

async fn store_one_card(store: &SqliteStore<ManualClock>) {
    let account = AccountId::try_from("acct").expect("valid account");
    let scope = SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::ContactCard,
    };
    let mut card = ContactCard::new(
        ContactId::try_from("card-1").expect("valid id"),
        Memberships::of_one(AddressBookId::try_from("book").expect("valid id")),
    );
    card.uid = Some(Uid::new("urn:uuid:card-1").expect("valid uid"));
    card.name = Some(ContactName {
        full: Some("Ada".into()),
        ..ContactName::default()
    });
    let claim = store
        .claim_sync_scope(
            account,
            &scope,
            LeaseRequest::new(WorkerId::new("w"), Duration::from_mins(5)),
        )
        .await
        .expect("claim");
    let present = std::iter::once(card.id.key().clone()).collect();
    let update = SyncUpdate::snapshot(vec![card], present);
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

/// Rewrites the stored card's `uid` as an earlier build could have left it, and reads it back.
async fn stored_as(path: &std::path::Path, uid: &serde_json::Value) -> Option<Uid> {
    rusqlite::Connection::open(path)
        .expect("raw open")
        .execute(
            "UPDATE object SET payload = json_set(payload, '$.uid', json(?1))",
            [uid.to_string()],
        )
        .expect("rewrite the stored uid");
    let sources = open(path).contact_sources().await.expect("the cards load");
    assert_eq!(sources.sources.len(), 1);
    sources.sources[0].card.uid.clone()
}

#[tokio::test]
async fn a_stored_uid_that_is_no_uid_loads_as_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("store.db");
    store_one_card(&open(&path)).await;

    assert_eq!(
        stored_as(&path, &serde_json::json!("urn:uuid:card-1")).await,
        Some(Uid::new("urn:uuid:card-1").unwrap())
    );
    assert_eq!(stored_as(&path, &serde_json::json!("")).await, None);
    let long = "u".repeat(Uid::MAX_OCTETS + 1);
    assert_eq!(stored_as(&path, &serde_json::json!(long)).await, None);
    // A provider id an earlier build stored as the UID is a string a `Uid` takes: it loads as one
    // until `NORMALIZER_VERSION` 18's re-snapshot re-reads the card without it.
    assert_eq!(
        stored_as(&path, &serde_json::json!("people/c1")).await,
        Some(Uid::new("people/c1").unwrap())
    );
}
