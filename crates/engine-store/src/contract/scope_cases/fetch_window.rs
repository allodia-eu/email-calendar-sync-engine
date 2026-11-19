//! The fetch window a windowed event scope records with its cursor.

use engine_core::{
    sync::{SyncState, SyncUpdate},
    time::{Horizon, UtcDateTime},
};

use super::super::{TestObject, acct, event_scope, lease_request};
use crate::{
    apply::{ApplyBatch, DerivedWrite},
    lease::ManualClock,
    read::StoreRead,
    store::Store,
};

/// The window commits with the cursor and only with it: a cursor apply carrying one records it,
/// a cursor apply carrying none (a delta, which keeps its cursor's window) leaves it as it was,
/// a streaming page moves nothing, and forgetting the scope drops it.
pub(in crate::contract) async fn fetch_window_travels_with_the_cursor<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-fetch-window");
    let scope = event_scope(&account);
    let empty = SyncUpdate::<TestObject>::delta(vec![], vec![]);
    let derived = DerivedWrite::empty();
    let cursor = SyncState::new("cursor-1");
    let narrow = window(2026, 2027);
    let wide = window(2025, 2028);

    assert_eq!(store.fetch_window(&scope).await.unwrap(), None);

    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 30))
        .await
        .unwrap();
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor).with_fetch_window(Some(narrow)),
        )
        .await
        .unwrap();
    assert_eq!(store.fetch_window(&scope).await.unwrap(), Some(narrow));

    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor),
        )
        .await
        .unwrap();
    assert_eq!(
        store.fetch_window(&scope).await.unwrap(),
        Some(narrow),
        "a delta continues its cursor's window, so it leaves the recorded one",
    );

    let page = SyncUpdate::delta(vec![TestObject::new("e1", "page")], vec![]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::with_cursor(&page, &derived, &[], None).with_fetch_window(Some(wide)),
        )
        .await
        .unwrap();
    assert_eq!(
        store.fetch_window(&scope).await.unwrap(),
        Some(narrow),
        "a streaming page moves neither the cursor nor its window",
    );

    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&empty, &derived, &[], &cursor).with_fetch_window(Some(wide)),
        )
        .await
        .unwrap();
    assert_eq!(store.fetch_window(&scope).await.unwrap(), Some(wide));

    store.forget_scope(claim.lease).await.unwrap();
    assert_eq!(store.fetch_window(&scope).await.unwrap(), None);
}

/// `[1 Jan from, 1 Jan to)` in UTC.
fn window(from: i32, to: i32) -> Horizon {
    let jan = |year| UtcDateTime::new(year, 1, 1, 0, 0, 0).unwrap();
    Horizon::new(jan(from), jan(to)).unwrap()
}
