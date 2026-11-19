//! The collection tag a scope records with its cursor.

use engine_core::{
    sync::{SyncState, SyncUpdate},
    version::CollectionTag,
};

use super::super::{TestObject, acct, email_scope, lease_request};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    read::StoreRead,
    store::Store,
};

/// The tag commits with the cursor: a cursor apply records it (or clears it, carrying none), a
/// streaming page leaves it as it was, and forgetting the scope drops it.
pub(in crate::contract) async fn collection_tag_travels_with_the_cursor<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-tag");
    let scope = email_scope(&account);
    let empty = SyncUpdate::<TestObject>::delta(vec![], vec![]);
    let derived = DerivedWrite::empty();
    let cursor = SyncState::new("cursor-1");
    let tag = CollectionTag::new("tag-1");

    assert_eq!(store.collection_tag(&scope).await.unwrap(), None);

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 30))
        .await
        .unwrap();
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor).with_collection_tag(Some(&tag)),
        )
        .await
        .unwrap();
    assert_eq!(
        store.collection_tag(&scope).await.unwrap(),
        Some(tag.clone())
    );

    let page = SyncUpdate::delta(vec![TestObject::new("m1", "page")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::with_cursor(&page, &derived, &[], None),
        )
        .await
        .unwrap();
    assert_eq!(
        store.collection_tag(&scope).await.unwrap(),
        Some(tag),
        "a streaming page moves neither the cursor nor its tag",
    );

    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor),
        )
        .await
        .unwrap();
    assert_eq!(
        store.collection_tag(&scope).await.unwrap(),
        None,
        "a cursor recorded without a tag clears the old one",
    );

    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor)
                .with_collection_tag(Some(&CollectionTag::new("tag-2"))),
        )
        .await
        .unwrap();
    store.forget_scope(claim.lease).await.unwrap();
    assert_eq!(store.collection_tag(&scope).await.unwrap(), None);
}
