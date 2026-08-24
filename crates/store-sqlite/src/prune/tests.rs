//! Unit tests for the local mail prune: it tombstones an account's out-of-window
//! mail (and its derived rows) exactly as a snapshot reconciliation would, keeps
//! in-window/boundary/undated mail, and never touches another account or a non-mail
//! (calendar) scope. The public async wrapper's unbounded no-op is covered too.

use engine_core::{
    ids::{AccountId, MailboxId},
    sync::{JmapDataType, SyncScope, SyncWindow},
    time::CalendarDate,
};
use engine_store::{ManualClock, PruneReport};
use rusqlite::Connection;

use super::prune_account_mail;
use crate::{FtsTokenizer, SqliteStore, convert};

/// The floor date the tests prune against: `2026-04-01`, inclusive.
fn floor() -> CalendarDate {
    CalendarDate::new(2026, 4, 1).unwrap()
}

fn account(id: &str) -> AccountId {
    AccountId::try_from(id).unwrap()
}

/// A JMAP mail scope for `account` (`search_domain() == Mail`).
fn mail_scope(account: &str) -> SyncScope {
    SyncScope::JmapType {
        account: self::account(account),
        data_type: JmapDataType::Email,
    }
}

/// A second **mail** scope for `account` — an IMAP mailbox, so one account can hold
/// the same provider key in two scopes (`search_domain() == Mail`).
fn imap_mail_scope(account: &str, mailbox: &str) -> SyncScope {
    SyncScope::ImapMailbox {
        account: self::account(account),
        mailbox: MailboxId::try_from(mailbox).unwrap(),
    }
}

/// A JMAP calendar scope for `account` (`search_domain() == Calendar`, so a prune
/// skips it).
fn calendar_scope(account: &str) -> SyncScope {
    SyncScope::JmapType {
        account: self::account(account),
        data_type: JmapDataType::CalendarEvent,
    }
}

/// Inserts the `sync_scope` row (with a cursor, so the prune must leave the cursor
/// untouched) and returns the canonical scope key the store enumerates it by.
fn seed_scope(conn: &Connection, scope: &SyncScope) -> String {
    let scope_key = convert::scope_key(scope);
    conn.execute(
        "INSERT INTO sync_scope (scope_key, account, token, cursor) VALUES (?1, ?2, 1, 'cur')",
        (&scope_key, scope.account().as_str()),
    )
    .unwrap();
    scope_key
}

/// Seeds one mail object into `scope_key` with its object payload, a full-text row
/// (subject term probeable via `fts_index MATCH`), a `message` row carrying
/// `date` (or `NULL`), one address + membership junction row, and both on-demand
/// content caches (body text and raw-source metadata) — so a tombstone can be
/// verified to clear *every* derived kind and both caches, not just the object.
fn seed_mail(conn: &Connection, scope_key: &str, key: &str, date: Option<&str>, subject: &str) {
    conn.execute(
        "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, ?2, '{}')",
        (scope_key, key),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
         VALUES (?1, ?2, ?3, '', '')",
        (scope_key, key, subject),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (scope_key, provider_key, account, date_utc, flags, has_attachment)
         VALUES (?1, ?2, (SELECT account FROM sync_scope WHERE scope_key = ?1), ?3, 0, 0)",
        (scope_key, key, date),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO mail_address (scope_key, provider_key, field, addr)
         VALUES (?1, ?2, 'from', 'x@example.test')",
        (scope_key, key),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO membership (scope_key, provider_key, kind, value)
         VALUES (?1, ?2, 'mailbox', 'inbox')",
        (scope_key, key),
    )
    .unwrap();
    seed_caches(conn, scope_key, key, subject);
}

/// Seeds the two lease-free content caches for one message: the extracted body text
/// (whose `message_body_fts` shadow a trigger maintains, so `subject` doubles as a
/// probeable body term) and the raw-source metadata pointing at a blob hash. Both are
/// keyed by `(account, provider_key)`, resolved from the scope.
fn seed_caches(conn: &Connection, scope_key: &str, key: &str, subject: &str) {
    conn.execute(
        "INSERT INTO message_body (account, provider_key, plain, fetched_at)
         VALUES ((SELECT account FROM sync_scope WHERE scope_key = ?1), ?2, ?3, '2026-06-01T00:00:00Z')",
        (scope_key, key, format!("body of {subject}")),
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message_source (account, provider_key, content_hash, fetched_at)
         VALUES ((SELECT account FROM sync_scope WHERE scope_key = ?1), ?2, ?3, '2026-06-01T00:00:00Z')",
        (scope_key, key, format!("hash-{key}")),
    )
    .unwrap();
}

/// Migrates a fresh database and seeds:
/// - account `a` mail: `old` (out), `edge` (on the floor, kept), `new` (in), `undated` (`NULL`
///   date, kept);
/// - account `b` mail: `b-old` (out — but a different account, so kept);
/// - account `a` calendar: `cal-old` (a non-mail scope, so kept), with an occurrence dated before
///   the floor.
///
/// Returns the connection and the three scope keys under test.
fn seed() -> (Connection, String, String, String) {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61).unwrap();

    let a_mail = seed_scope(&conn, &mail_scope("a"));
    seed_mail(&conn, &a_mail, "old", Some("2026-01-15T09:00:00Z"), "alpha");
    seed_mail(
        &conn,
        &a_mail,
        "edge",
        Some("2026-04-01T00:00:00Z"),
        "edgeterm",
    );
    seed_mail(
        &conn,
        &a_mail,
        "new",
        Some("2026-06-20T09:00:00Z"),
        "newterm",
    );
    seed_mail(&conn, &a_mail, "undated", None, "undatedterm");

    let b_mail = seed_scope(&conn, &mail_scope("b"));
    seed_mail(
        &conn,
        &b_mail,
        "b-old",
        Some("2026-01-10T09:00:00Z"),
        "beta",
    );

    let a_cal = seed_scope(&conn, &calendar_scope("a"));
    conn.execute(
        "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, 'cal-old', '{}')",
        [&a_cal],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO event_occurrence
             (scope_key, event, start_utc, end_utc, recurrence_id, tzdata_version)
         VALUES (?1, 'cal-old', '2026-01-05T00:00:00Z', '2026-01-05T01:00:00Z', '', '2025a')",
        [&a_cal],
    )
    .unwrap();

    (conn, a_mail, b_mail, a_cal)
}

/// Counts rows in `table` for one `(scope_key, provider_key)`.
fn count(conn: &Connection, table: &str, scope_key: &str, key: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE scope_key = ?1 AND provider_key = ?2"),
        (scope_key, key),
        |r| r.get(0),
    )
    .unwrap()
}

/// Counts `fts_index` rows matching `term` (proving the FTS5 shadow followed the
/// `fts_doc` delete through its trigger).
fn fts_matches(conn: &Connection, term: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM fts_index WHERE fts_index MATCH ?1",
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn removes_only_mail_dated_before_the_floor() {
    let (mut conn, a_mail, ..) = seed();
    let report = prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    // Exactly the one out-of-window message is removed.
    assert_eq!(
        report,
        PruneReport {
            messages_removed: 1
        }
    );
    assert_eq!(count(&conn, "object", &a_mail, "old"), 0, "old kept");
    // Boundary (== floor, inclusive), later, and undated mail are all kept.
    for kept in ["edge", "new", "undated"] {
        assert_eq!(count(&conn, "object", &a_mail, kept), 1, "{kept} dropped");
        assert_eq!(
            count(&conn, "message", &a_mail, kept),
            1,
            "{kept} index dropped"
        );
    }
    // The cursor is untouched — a prune advances no sync state.
    let cursor: Option<String> = conn
        .query_row(
            "SELECT cursor FROM sync_scope WHERE scope_key = ?1",
            [&a_mail],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor.as_deref(), Some("cur"));
}

#[test]
fn tombstone_clears_every_derived_kind_for_the_pruned_mail() {
    let (mut conn, a_mail, ..) = seed();
    prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    // The pruned message's object and each derived kind are gone; its neighbours keep
    // theirs, so the delete was surgical.
    for table in ["object", "fts_doc", "message", "mail_address", "membership"] {
        assert_eq!(count(&conn, table, &a_mail, "old"), 0, "{table} kept old");
        assert_eq!(count(&conn, table, &a_mail, "new"), 1, "{table} lost new");
    }
    // The FTS5 shadow followed the base-table delete (old's term gone, new's kept).
    assert_eq!(fts_matches(&conn, "alpha"), 0);
    assert_eq!(fts_matches(&conn, "newterm"), 1);
}

#[test]
fn leaves_other_accounts_untouched() {
    let (mut conn, _, b_mail, _) = seed();
    prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    // b-old is out of window but belongs to account b, so pruning a must keep it.
    assert_eq!(count(&conn, "object", &b_mail, "b-old"), 1);
    assert_eq!(count(&conn, "message", &b_mail, "b-old"), 1);
    assert_eq!(fts_matches(&conn, "beta"), 1);
}

#[test]
fn skips_non_mail_scopes() {
    let (mut conn, _, _, a_cal) = seed();
    let report = prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    // The calendar object is dated before the floor but lives in a non-mail scope, so
    // the mail prune never considers it — nor its occurrence.
    assert_eq!(report.messages_removed, 1);
    assert_eq!(count(&conn, "object", &a_cal, "cal-old"), 1);
    let occurrences: i64 = conn
        .query_row(
            "SELECT count(*) FROM event_occurrence WHERE scope_key = ?1",
            [&a_cal],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(occurrences, 1);
}

#[test]
fn pruning_an_unknown_account_removes_nothing() {
    let (mut conn, a_mail, ..) = seed();
    let report =
        prune_account_mail(&mut conn, &account("never-synced"), &floor().to_string()).unwrap();
    assert_eq!(report, PruneReport::default());
    assert_eq!(count(&conn, "object", &a_mail, "old"), 1);
}

#[tokio::test]
async fn unbounded_window_is_a_noop() {
    // The public wrapper short-circuits a full (floorless) window before touching the
    // store — nothing is "outside" an unbounded window.
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap()))
            .unwrap();
    let report = store
        .prune_account_mail_outside_window(&account("a"), SyncWindow::full())
        .await
        .unwrap();
    assert_eq!(report, PruneReport::default());
}

#[tokio::test]
async fn bounded_prune_on_an_empty_store_removes_nothing() {
    // Drives the async wrapper end to end: a bounded window over an account the store
    // never synced enumerates no scopes and removes nothing.
    let store =
        SqliteStore::open_in_memory(ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap()))
            .unwrap();
    let report = store
        .prune_account_mail_outside_window(&account("a"), SyncWindow::since(floor()))
        .await
        .unwrap();
    assert_eq!(report, PruneReport::default());
}

/// Counts rows in an account-keyed content cache for one `(account, provider_key)`.
fn cached(conn: &Connection, table: &str, account: &str, key: &str) -> i64 {
    conn.query_row(
        &format!("SELECT count(*) FROM {table} WHERE account = ?1 AND provider_key = ?2"),
        (account, key),
        |r| r.get(0),
    )
    .unwrap()
}

/// Counts `message_body_fts` rows matching `term` (proving the FTS5 shadow followed
/// the `message_body` delete through its trigger).
fn body_matches(conn: &Connection, term: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM message_body_fts WHERE message_body_fts MATCH ?1",
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn pruned_mail_takes_its_cached_body_and_source_with_it() {
    let (mut conn, ..) = seed();
    prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    // The caches are the bulk of what an account occupies on disk, so they follow the
    // message out of the window rather than outliving it.
    for table in ["message_body", "message_source"] {
        assert_eq!(cached(&conn, table, "a", "old"), 0, "{table} kept old");
        assert_eq!(cached(&conn, table, "a", "new"), 1, "{table} lost new");
    }
    assert_eq!(body_matches(&conn, "alpha"), 0);
    assert_eq!(body_matches(&conn, "newterm"), 1);
}

#[test]
fn a_copy_still_live_in_another_scope_keeps_its_cached_body_and_source() {
    let (mut conn, ..) = seed();
    // The caches are keyed by `(account, provider_key)` while a tombstone is per scope,
    // so a provider whose key spans folders would lose a live message's body when one
    // folder dropped its copy.
    let second = seed_scope(&conn, &imap_mail_scope("a", "archive"));
    conn.execute(
        "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, 'old', '{}')",
        [&second],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (scope_key, provider_key, account, date_utc, flags, has_attachment)
         VALUES (?1, 'old', 'a', '2026-06-20T09:00:00Z', 0, 0)",
        [&second],
    )
    .unwrap();

    prune_account_mail(&mut conn, &account("a"), &floor().to_string()).unwrap();

    for table in ["message_body", "message_source"] {
        assert_eq!(cached(&conn, table, "a", "old"), 1, "{table} dropped old");
    }
    assert_eq!(body_matches(&conn, "alpha"), 1);
}
