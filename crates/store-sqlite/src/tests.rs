//! Unit tests for the crate-root store wiring: `Debug` redaction, the
//! normalizer-version / per-scope cursor-clear reconciliation, and the FTS
//! tokenizer record-and-refuse reconciliation.

use engine_store::ManualClock;

use super::SqliteStore;
use crate::{
    options::{FtsTokenizer, OpenOptions},
    tokenizer_reconcile::FtsTokenizerKnown,
};

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
    crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();

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
    crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();

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

    crate::scope_ops::clear_one_cursor(&conn, "s").unwrap();

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

#[tokio::test]
async fn a_file_store_reads_through_a_connection_that_cannot_write() {
    // `query_only` on the readers is what makes the read/write routing checkable at
    // all: without it a write handed to `read` would quietly take a reader's lock,
    // succeed, and leave the split looking correct while it silently serialized
    // again. The on-disk contract run is the gate this pragma arms.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SqliteStore::open(
        dir.path().join("readers.sqlite"),
        ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant")),
    )
    .expect("open file store");

    let insert = "INSERT INTO meta (key, value) VALUES ('probe', '1')";
    let refused = store
        .read(move |conn| conn.execute(insert, []).map_err(|err| err.to_string()))
        .await;
    assert!(
        refused.is_err_and(|err| err.contains("readonly")),
        "a reader must refuse a write outright"
    );

    // The same statement on the writer succeeds, so the refusal above is the routing
    // and not a broken schema.
    store
        .call(move |conn| conn.execute(insert, []).expect("the writer accepts it"))
        .await;
    let stored = store
        .read(|conn| {
            conn.query_row("SELECT value FROM meta WHERE key = 'probe'", [], |row| {
                row.get::<_, String>(0)
            })
            .expect("read it back")
        })
        .await;
    assert_eq!(stored, "1", "a reader sees the writer's committed row");
}

#[test]
fn fresh_database_uses_the_requested_tokenizer_for_both_fts_tables() {
    for (tokenizer, clause) in [
        (FtsTokenizer::Trigram, "trigram"),
        (FtsTokenizer::PorterUnicode61, "porter unicode61"),
    ] {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, tokenizer).unwrap();
        for table in ["fts_index", "message_body_fts"] {
            let ddl: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                ddl.contains(&format!("tokenize = '{clause}'")),
                "{table} under {clause}: {ddl}"
            );
        }
    }
}

/// `Fresh` = the meta table itself is absent (a database this open creates).
#[test]
fn a_fresh_database_records_the_requested_tokenizer() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    super::reconcile_fts_tokenizer(FtsTokenizerKnown::Fresh, &conn, FtsTokenizer::Trigram).unwrap();
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, "trigram");
}

/// A database older than the option was necessarily created porter unicode61.
#[test]
fn a_pre_option_database_is_recorded_as_porter_and_refuses_trigram() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();
    // Simulate pre-option: the meta table exists, the row does not.
    let recorded = super::reconcile_fts_tokenizer(
        FtsTokenizerKnown::PreOption,
        &conn,
        FtsTokenizer::PorterUnicode61,
    );
    assert!(recorded.is_ok());
    let refused =
        super::reconcile_fts_tokenizer(FtsTokenizerKnown::PreOption, &conn, FtsTokenizer::Trigram);
    let msg = format!("{}", refused.unwrap_err());
    assert!(msg.contains("fts tokenizer mismatch"), "{msg}");
}

#[test]
fn a_recorded_tokenizer_mismatching_the_request_is_refused() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('fts_tokenizer', 'trigram')",
        [],
    )
    .unwrap();
    let refused = super::reconcile_fts_tokenizer(
        FtsTokenizerKnown::Known(FtsTokenizer::Trigram),
        &conn,
        FtsTokenizer::PorterUnicode61,
    );
    assert!(refused.is_err());
    // Re-requesting the recorded value stays a no-op.
    super::reconcile_fts_tokenizer(
        FtsTokenizerKnown::Known(FtsTokenizer::Trigram),
        &conn,
        FtsTokenizer::Trigram,
    )
    .unwrap();
}

/// The `_with` constructors must thread the option all the way through
/// `configure`: the FTS tables this open creates carry the requested tokenizer
/// and the choice is recorded in `meta`. An in-memory database vanishes with
/// its connection, so construction succeeding under a non-default option —
/// with the schema and record to show for it — is this test's assertion; the
/// mismatch refusal itself is covered at the connection level above.
#[tokio::test]
async fn open_in_memory_with_trigram_creates_and_records_the_trigram_index() {
    let store = SqliteStore::open_in_memory_with(
        ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant")),
        OpenOptions {
            fts_tokenizer: FtsTokenizer::Trigram,
        },
    )
    .expect("open under the trigram option");
    let (ddl, recorded): (String, String) = store
        .read(|conn| {
            let ddl = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'fts_index'",
                    [],
                    |r| r.get(0),
                )
                .expect("fts_index exists");
            let recorded = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = 'fts_tokenizer'",
                    [],
                    |r| r.get(0),
                )
                .expect("the tokenizer row is recorded");
            (ddl, recorded)
        })
        .await;
    assert!(ddl.contains("tokenize = 'trigram'"), "{ddl}");
    assert_eq!(recorded, "trigram");
}

/// The kylins CJK acceptance case (spec P0 §4): a mid-string query must match
/// under `trigram`. This is the search-as-you-type phrase-prefix form the
/// search layer really issues (`fts_match`), not a hand-rolled MATCH.
#[test]
fn trigram_matches_mid_string_cjk_where_porter_cannot() {
    let body = "请查收今天的会议纪要附件";
    let query = "\"会议纪\"*";
    for (tokenizer, expected) in [
        (FtsTokenizer::Trigram, 1),
        (FtsTokenizer::PorterUnicode61, 0),
    ] {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn, tokenizer).unwrap();
        conn.execute(
            "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
             VALUES ('s', 'm1', '周报', ?1, '会议室 3A')",
            [body],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM fts_index WHERE fts_index MATCH ?1",
                [query],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, expected, "{tokenizer:?} on query {query}");
    }
}

/// The ≥3-character rule is part of the contract: a 2-character query cannot
/// use a trigram index (kylins' previous engine behaves the same way — no
/// regression, now documented, spec P0 §4).
#[test]
fn trigram_two_character_queries_do_not_match() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::Trigram).unwrap();
    conn.execute(
        "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
         VALUES ('s', 'm1', '周报', '请查收今天的会议纪要附件', '')",
        [],
    )
    .unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM fts_index WHERE fts_index MATCH ?1",
            ["\"会议\"*"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 0);
}
