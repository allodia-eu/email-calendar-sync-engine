//! Unit tests for [`purge_account`]: it drops exactly one account's rows across the
//! scope-keyed and account-keyed tables (the FTS5 shadows follow via triggers) and
//! leaves every other account intact.

use rusqlite::Connection;

use super::purge_account;

/// Migrates a fresh in-memory database and seeds two accounts (`a`, `b`) with one
/// object each across every table `purge_account` touches, so a purge of `a` must
/// leave `b` untouched. Body text is a distinct FTS term per account (`alpha`/`beta`
/// for the search index, `gamma`/`delta` for the message body) so the FTS5 shadows
/// can be probed by `MATCH`.
fn seed_two_accounts() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn).unwrap();
    for (account, scope, key, subject_term, body_term) in [
        ("a", "sa", "k1", "alpha", "gamma"),
        ("b", "sb", "k2", "beta", "delta"),
    ] {
        conn.execute(
            "INSERT INTO sync_scope (scope_key, account, token, cursor) VALUES (?1, ?2, 1, 'c')",
            (scope, account),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, ?2, '{}')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
             VALUES (?1, ?2, ?3, '', '')",
            (scope, key, subject_term),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (scope_key, provider_key, account, flags, has_attachment)
             VALUES (?1, ?2, (SELECT account FROM sync_scope WHERE scope_key = ?1), 0, 0)",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_address (scope_key, provider_key, field, addr)
             VALUES (?1, ?2, 'from', 'x@example.test')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, 'mailbox', 'inbox')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event_occurrence
                 (scope_key, event, start_utc, end_utc, recurrence_id, tzdata_version)
             VALUES (?1, ?2, '2026-01-01T00:00:00Z', '2026-01-01T01:00:00Z', '', '2025a')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event_index (scope_key, provider_key, has_conference) VALUES (?1, ?2, 0)",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event_participant (scope_key, provider_key, role, addr, partstat)
             VALUES (?1, ?2, 'req', 'p@example.test', 'accepted')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embedding (scope_key, provider_key, chunk_ix, model, dim, vector)
             VALUES (?1, ?2, 0, 'model', 1, X'00')",
            (scope, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pending_op
                 (account, idempotency_key, resource_key, depends_on, payload, state, token)
             VALUES (?1, 'idem', 'res', '[]', '{}', 'Queued', 1)",
            [account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_source (account, provider_key, content_hash, fetched_at)
             VALUES (?1, ?2, 'hash', '2026-01-01T00:00:00Z')",
            (account, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_body (account, provider_key, plain, fetched_at)
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z')",
            (account, key, body_term),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contact_source_availability (scope_key, available)
             VALUES (?1, 1)",
            [scope],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contact_photo
                 (account, contact, resource, fingerprint, content_hash, fetched_at)
             VALUES (?1, ?2, 'res-1', 'revision', 'hash', '2026-01-01T00:00:00Z')",
            (account, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recipient_observation
                 (account, source_message, email, name, sent_at)
             VALUES (?1, ?2, 'recipient@example.test', 'Recipient',
                     '2026-01-01T00:00:00Z')",
            (account, key),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recipient_coverage
                 (account, window_json, sent_collection_present)
             VALUES (?1, '{\"kind\":\"full\"}', 1)",
            [account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recipient_index_state (account, version) VALUES (?1, 1)",
            [account],
        )
        .unwrap();
    }
    conn
}

/// Counts the rows in `table` for one scope or account key column.
fn count(conn: &Connection, table: &str, column: &str, value: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE {column} = ?1"),
        [value],
        |r| r.get(0),
    )
    .unwrap()
}

/// Counts the FTS5 rows matching `term` (proving the external-content shadow tracked
/// the base-table delete through its trigger).
fn fts_matches(conn: &Connection, table: &str, term: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE {table} MATCH ?1"),
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn purge_account_drops_every_table_for_the_account() {
    let mut conn = seed_two_accounts();
    purge_account(&mut conn, "a").unwrap();

    // Scope-keyed tables: account a's scope is gone (every one leads on scope_key,
    // event_occurrence included — its per-object column is `event`, not the scope).
    for table in super::SCOPE_TABLES {
        assert_eq!(
            count(&conn, table, "scope_key", "sa"),
            0,
            "{table} kept sa rows"
        );
    }
    // Account-keyed tables plus sync_scope.
    for table in super::ACCOUNT_TABLES {
        assert_eq!(
            count(&conn, table, "account", "a"),
            0,
            "{table} kept a rows"
        );
    }
    assert_eq!(count(&conn, "sync_scope", "account", "a"), 0);

    // The FTS5 shadows followed the base-table deletes via their triggers.
    assert_eq!(fts_matches(&conn, "fts_index", "alpha"), 0);
    assert_eq!(fts_matches(&conn, "message_body_fts", "gamma"), 0);
}

#[test]
fn purge_account_leaves_other_accounts_intact() {
    let mut conn = seed_two_accounts();
    purge_account(&mut conn, "a").unwrap();

    // Account b is untouched across every table (and its FTS shadows still match).
    for table in super::SCOPE_TABLES {
        assert_eq!(count(&conn, table, "scope_key", "sb"), 1, "{table} lost sb");
    }
    for table in super::ACCOUNT_TABLES {
        assert_eq!(count(&conn, table, "account", "b"), 1, "{table} lost b");
    }
    assert_eq!(count(&conn, "sync_scope", "account", "b"), 1);
    assert_eq!(fts_matches(&conn, "fts_index", "beta"), 1);
    assert_eq!(fts_matches(&conn, "message_body_fts", "delta"), 1);
}

#[test]
fn purge_account_is_a_noop_for_an_unknown_account() {
    let mut conn = seed_two_accounts();
    // Forgetting an account the store never knew touches nothing and does not error.
    purge_account(&mut conn, "never-synced").unwrap();
    assert_eq!(count(&conn, "sync_scope", "account", "a"), 1);
    assert_eq!(count(&conn, "sync_scope", "account", "b"), 1);
}
