//! Unit tests for the crate-root store wiring: `Debug` redaction and the
//! normalizer-version / per-scope cursor-clear reconciliation.

use engine_store::ManualClock;

use super::SqliteStore;

#[test]
fn debug_is_redacted() {
    // The Debug form must not expose the connection (it may map sensitive data).
    let store = SqliteStore::open_in_memory(ManualClock::new(
        "2026-01-01T00:00:00Z".parse().expect("valid instant"),
    ))
    .expect("open");
    let rendered = format!("{store:?}");
    assert!(rendered.contains("SqliteStore"));
    assert!(rendered.contains(".."));
}

#[test]
fn a_normalizer_version_change_clears_sync_cursors() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn).unwrap();

    // A synced scope carries a cursor; reconciling at the same version keeps it.
    super::reconcile_normalizer_version(&conn, 1).unwrap();
    conn.execute(
        "INSERT INTO sync_scope (scope_key, account, token, cursor) VALUES ('s', 'a', 1, 'c1')",
        [],
    )
    .unwrap();
    super::reconcile_normalizer_version(&conn, 1).unwrap();
    let cursor: Option<String> = conn
        .query_row(
            "SELECT cursor FROM sync_scope WHERE scope_key = 's'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursor.as_deref(),
        Some("c1"),
        "unchanged version keeps cursors"
    );

    // A bump clears the cursor, so the next sync re-snapshots + re-normalizes.
    super::reconcile_normalizer_version(&conn, 2).unwrap();
    let cursor: Option<String> = conn
        .query_row(
            "SELECT cursor FROM sync_scope WHERE scope_key = 's'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, None, "a version bump clears cursors");
}

#[test]
fn clear_one_cursor_clears_the_cursor_but_keeps_a_held_lease() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn).unwrap();

    // A scope mid-sync: a cursor plus a live lease (a fencing token and a future
    // expiry). The per-scope clear runs concurrently with such syncs, so unlike
    // reset_sync it must clear ONLY the cursor — stealing the lease would let the
    // in-flight worker commit its cursor back over the clear.
    conn.execute(
        "INSERT INTO sync_scope (scope_key, account, token, cursor, lease_expiry) \
             VALUES ('s', 'a', 5, 'c1', '2099-01-01T00:00:00Z')",
        [],
    )
    .unwrap();

    super::clear_one_cursor(&conn, "s").unwrap();

    let (cursor, token, lease): (Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT cursor, token, lease_expiry FROM sync_scope WHERE scope_key = 's'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        cursor, None,
        "the cursor is cleared so the next sync snapshots"
    );
    assert_eq!(token, 5, "the fencing token is untouched");
    assert_eq!(
        lease.as_deref(),
        Some("2099-01-01T00:00:00Z"),
        "a live lease is NOT stolen (the contrast with reset_sync)"
    );
}

#[tokio::test]
async fn the_expansion_window_round_trips_and_is_lease_gated() {
    use core::time::Duration;

    use engine_core::{
        ids::AccountId,
        sync::{JmapDataType, SyncScope},
        time::{ExpansionWindow, Horizon, TimeZoneId},
    };
    use engine_store::{LeaseRequest, Store, StoreError, StoreRead, WorkerId};

    let store = SqliteStore::open_in_memory(ManualClock::new(
        "2026-01-01T00:00:00Z".parse().expect("valid instant"),
    ))
    .expect("open");
    let account = AccountId::try_from("acct-1").unwrap();
    let scope = SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::CalendarEvent,
    };
    let window = ExpansionWindow::new(
        Horizon::new(
            "2026-01-01T00:00:00Z".parse().unwrap(),
            "2026-12-31T00:00:00Z".parse().unwrap(),
        )
        .unwrap(),
        TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    );

    // A scope nothing has expanded has no window — which is what makes a reconcile before
    // the first sync refusable rather than a silently empty calendar.
    assert_eq!(store.expansion_window(&scope).await.unwrap(), None);

    let req = LeaseRequest::new(WorkerId::new("w-1"), Duration::from_mins(1));
    let claim = store
        .claim_sync_scope(account.clone(), &scope, req.clone())
        .await
        .unwrap();
    store
        .set_expansion_window(&claim.lease, &window)
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    assert_eq!(
        store.expansion_window(&scope).await.unwrap(),
        Some(window.clone()),
        "the horizon and the zone both survive the round trip"
    );

    // It is written under the scope's fencing token, exactly like the rows it describes: a
    // worker whose lease has been superseded cannot move the window out from under the one
    // that owns the scope now.
    let superseded = store.claim_sync_scope(account, &scope, req).await.unwrap();
    store.abandon_sync_leases().await.unwrap();
    assert!(matches!(
        store.set_expansion_window(&superseded.lease, &window).await,
        Err(StoreError::StaleLease)
    ));
}
