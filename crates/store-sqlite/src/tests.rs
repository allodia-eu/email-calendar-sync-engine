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
