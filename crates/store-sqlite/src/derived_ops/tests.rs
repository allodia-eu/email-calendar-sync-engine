//! Unit tests for derived-row apply: tombstone cascade, idempotent occurrence
//! replay, `removed` clears, and re-expansion version/instant stability.

use engine_core::{
    ids::{AccountId, ProviderKey},
    search_index::FtsRow,
    sync::{JmapDataType, SyncScope},
    time::UtcDateTime,
};
use engine_store::{FtsField, OccurrenceRow, TzdataVersion, WorkerId};
use rusqlite::Connection;

use super::*;
use crate::scope_ops::{OwnedUpdate, apply, claim, maintenance};

fn instant(text: &str) -> UtcDateTime {
    text.parse().expect("valid instant")
}

fn pk(value: &str) -> ProviderKey {
    ProviderKey::new(value).expect("valid key")
}

fn events_scope() -> SyncScope {
    SyncScope::JmapType {
        account: AccountId::try_from("a").expect("valid account"),
        data_type: JmapDataType::CalendarEvent,
    }
}

fn open() -> (Connection, String) {
    let mut conn = Connection::open_in_memory().expect("open");
    crate::migrations::migrate(&mut conn).expect("schema");
    (conn, convert::scope_key(&events_scope()))
}

fn claim_token(conn: &mut Connection, key: &str) -> u64 {
    claim(
        conn,
        AccountId::try_from("a").unwrap(),
        events_scope(),
        key,
        WorkerId::new("w"),
        instant("2026-01-01T00:00:00Z"),
        instant("2026-01-01T00:05:00Z"),
    )
    .expect("claim")
    .lease
    .token()
    .get()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

fn delta_change(key: &str) -> OwnedUpdate {
    OwnedUpdate::Delta {
        changed: vec![(key.to_owned(), "{}".to_owned())],
        removed: Vec::new(),
    }
}

fn occurrence(event: &str) -> OccurrenceRow {
    OccurrenceRow {
        event: pk(event),
        start: instant("2026-03-01T09:00:00Z"),
        end: instant("2026-03-01T09:15:00Z"),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    }
}

#[test]
fn fts_columns_route_by_field_name() {
    let (subject, body, location) = fts_columns(&[
        FtsField::new("subject", "Quarterly"),
        FtsField::new("body", "see"),
        FtsField::new("attachment", "report"), // unknown → body
        FtsField::new("location", "Room 4"),
    ]);
    assert_eq!(subject, "Quarterly");
    assert_eq!(body, "see report"); // unknown field folded into body, space-joined
    assert_eq!(location, "Room 4");
}

#[test]
fn tombstoning_an_object_cascades_to_its_derived_rows() {
    let (mut conn, key) = open();
    let token = claim_token(&mut conn, &key);

    let mut derived = DerivedWrite::empty();
    derived.fts.push(FtsRow::new(
        pk("e1"),
        vec![FtsField::new("subject", "standup")],
    ));
    derived.occurrences.push(occurrence("e1"));
    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &derived,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM object"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_doc"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_index"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 1);

    let remove = OwnedUpdate::Delta {
        changed: Vec::new(),
        removed: vec!["e1".to_owned()],
    };
    let applied = apply(
        &mut conn,
        &key,
        token,
        &remove,
        &DerivedWrite::empty(),
        &[],
        &[],
        false,
        Some("c2"),
    )
    .unwrap();
    assert_eq!(applied.tombstoned, 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM object"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_doc"), 0);
    // The external-content FTS index is cleared by the delete trigger.
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_index"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 0);
}

#[test]
fn replaying_occurrences_does_not_duplicate_rows() {
    let (mut conn, key) = open();
    let token = claim_token(&mut conn, &key);
    let mut derived = DerivedWrite::empty();
    derived.occurrences.push(occurrence("e1"));

    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &derived,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();
    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &derived,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 1);
}

#[test]
fn removed_derived_keys_clear_rows_but_keep_the_object() {
    let (mut conn, key) = open();
    let token = claim_token(&mut conn, &key);
    let mut derived = DerivedWrite::empty();
    derived
        .fts
        .push(FtsRow::new(pk("e1"), vec![FtsField::new("subject", "x")]));
    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &derived,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_doc"), 1);

    let mut clear = DerivedWrite::empty();
    clear.removed.push(pk("e1"));
    maintenance(&mut conn, &key, token, &clear).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM object"), 1);
    assert_eq!(count(&conn, "SELECT count(*) FROM fts_doc"), 0);
}

#[test]
fn overridden_and_base_occurrences_coexist() {
    let (mut conn, key) = open();
    let token = claim_token(&mut conn, &key);
    let mut derived = DerivedWrite::empty();
    derived.occurrences.push(occurrence("e1"));
    derived.occurrences.push(OccurrenceRow {
        event: pk("e1"),
        start: instant("2026-03-01T09:00:00Z"),
        end: instant("2026-03-01T10:00:00Z"),
        recurrence_id: Some(instant("2026-03-01T09:00:00Z")),
        tzdata_version: TzdataVersion::new("2025b"),
    });
    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &derived,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 2);
}

#[test]
fn re_expansion_updates_version_and_keeps_instants_byte_stable() {
    // A tzdata bump re-expands an event: a single maintenance batch clears the
    // stale occurrence and writes a fresh one. A zone whose rules did not change
    // resolves to the same instants, so only `tzdata_version` changes.
    let (mut conn, key) = open();
    let token = claim_token(&mut conn, &key);

    let mut initial = DerivedWrite::empty();
    initial.occurrences.push(OccurrenceRow {
        event: pk("e1"),
        start: instant("2026-03-01T09:00:00Z"),
        end: instant("2026-03-01T09:15:00Z"),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025a"),
    });
    apply(
        &mut conn,
        &key,
        token,
        &delta_change("e1"),
        &initial,
        &[],
        &[],
        false,
        Some("c1"),
    )
    .unwrap();

    let mut re_expand = DerivedWrite::empty();
    re_expand.removed.push(pk("e1"));
    re_expand.occurrences.push(OccurrenceRow {
        event: pk("e1"),
        start: instant("2026-03-01T09:00:00Z"),
        end: instant("2026-03-01T09:15:00Z"),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    });
    maintenance(&mut conn, &key, token, &re_expand).unwrap();

    assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 1);
    let (start, end, version): (String, String, String) = conn
        .query_row(
            "SELECT start_utc, end_utc, tzdata_version FROM event_occurrence",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(start, "2026-03-01T09:00:00Z");
    assert_eq!(end, "2026-03-01T09:15:00Z");
    assert_eq!(version, "2025b");
}

/// A `MailRow` carrying only what the size rule needs.
fn sized_row(key: &str, size: Option<u64>) -> engine_core::search_index::MailRow {
    engine_core::search_index::MailRow {
        key: pk(key),
        thread_id: None,
        message_id: None,
        date_utc: None,
        flags: engine_core::mail::MailFlags::default(),
        has_attachment: false,
        size_octets: size,
        from_name: None,
        from_addr: None,
        subject: None,
        preview: None,
        revisions: engine_core::version::RevisionTokens::default(),
        last_modified: None,
    }
}

fn stored_size(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT size_octets FROM message", [], |r| r.get(0))
        .expect("one message row")
}

#[test]
fn a_reported_size_is_stored_and_a_silent_re_fetch_does_not_erase_it() {
    // The same "nothing to say, not clear it" rule `thread_id` and `preview` follow, and the
    // one that matters most here: Graph reports no size at all, so an account that syncs the
    // same message through two adapters — or an adapter that starts reporting and stops —
    // must not lose the number a size cap decides on.
    let mut conn = Connection::open_in_memory().expect("open");
    crate::migrations::migrate(&mut conn).expect("schema");

    let tx = conn.transaction().expect("tx");
    mail::upsert_message(&tx, "s1", "acct", &sized_row("m1", Some(4_194_304))).expect("insert");
    tx.commit().expect("commit");
    assert_eq!(stored_size(&conn), Some(4_194_304));

    let tx = conn.transaction().expect("tx");
    mail::upsert_message(&tx, "s1", "acct", &sized_row("m1", None)).expect("re-fetch");
    tx.commit().expect("commit");
    assert_eq!(
        stored_size(&conn),
        Some(4_194_304),
        "an adapter with no opinion must not clear a size another one reported",
    );

    let tx = conn.transaction().expect("tx");
    mail::upsert_message(&tx, "s1", "acct", &sized_row("m1", Some(512))).expect("re-fetch");
    tx.commit().expect("commit");
    assert_eq!(stored_size(&conn), Some(512), "a new number still wins");
}
