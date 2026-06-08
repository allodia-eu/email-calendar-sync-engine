//! Writing, clearing, and counting the precomputed derived rows.
//!
//! The store is mechanical: these functions persist the [`DerivedWrite`] the
//! engine computed (`store-and-sync.md`), they never derive anything. Full-text
//! text maps onto the `fts_doc` external-content columns (the FTS5 index follows
//! via triggers); scalar rows upsert; junction rows *replace* per object so a
//! re-projection drops stale rows and a replay is idempotent; `removed` and
//! tombstoning clear every kind together.

use std::collections::HashSet;

use engine_core::calendar::ParticipationStatus;
use engine_core::ids::ThreadId;
use engine_core::search_index::{EventParticipantRow, FtsField, MailAddressRow, MembershipRow};
use engine_store::{DerivedWrite, IndexRowCounts, Result};
use rusqlite::{Connection, Transaction};

use crate::convert;

/// Applies the precomputed derived rows for one scope inside the apply/maintenance
/// transaction.
///
/// `removed` is cleared **first**, then the upserts, so a single re-expansion batch
/// (`{removed: [event], occurrences: [fresh]}`) clears the stale occurrences and
/// writes the fresh ones in one transaction without the clear wiping the new rows.
pub(crate) fn apply_derived(
    tx: &Transaction<'_>,
    scope_key: &str,
    derived: &DerivedWrite,
) -> Result<()> {
    for key in &derived.removed {
        delete_derived_rows(tx, scope_key, key.as_str())?;
    }
    for row in &derived.fts {
        let (subject, body, location) = fts_columns(&row.fields);
        tx.execute(
            "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key) DO UPDATE SET
                 subject = excluded.subject, body = excluded.body, location = excluded.location",
            (scope_key, row.key.as_str(), subject, body, location),
        )
        .map_err(convert::backend)?;
    }
    for occ in &derived.occurrences {
        let recurrence_id = occ
            .recurrence_id
            .map(convert::instant_to_text)
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO event_occurrence
                 (scope_key, event, start_utc, end_utc, recurrence_id, tzdata_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(scope_key, event, start_utc, recurrence_id)
             DO UPDATE SET end_utc = excluded.end_utc, tzdata_version = excluded.tzdata_version",
            (
                scope_key,
                occ.event.as_str(),
                convert::instant_to_text(occ.start),
                convert::instant_to_text(occ.end),
                recurrence_id,
                occ.tzdata_version.as_str(),
            ),
        )
        .map_err(convert::backend)?;
    }
    for row in &derived.mail_index {
        tx.execute(
            "INSERT INTO mail_index (scope_key, provider_key, date_utc, has_attachment, thread_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key) DO UPDATE SET
                 date_utc = excluded.date_utc,
                 has_attachment = excluded.has_attachment,
                 thread_id = excluded.thread_id",
            (
                scope_key,
                row.key.as_str(),
                row.date_utc.map(convert::instant_to_text),
                i64::from(row.has_attachment),
                row.thread_id.as_ref().map(ThreadId::as_str),
            ),
        )
        .map_err(convert::backend)?;
    }
    for row in &derived.event_index {
        tx.execute(
            "INSERT INTO event_index (scope_key, provider_key, has_conference, my_partstat)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, provider_key) DO UPDATE SET
                 has_conference = excluded.has_conference,
                 my_partstat = excluded.my_partstat",
            (
                scope_key,
                row.key.as_str(),
                i64::from(row.has_conference),
                row.my_partstat.as_ref().map(ParticipationStatus::as_str),
            ),
        )
        .map_err(convert::backend)?;
    }
    replace_addresses(tx, scope_key, &derived.addresses)?;
    replace_memberships(tx, scope_key, &derived.memberships)?;
    replace_participants(tx, scope_key, &derived.participants)?;
    Ok(())
}

/// Removes every derived row kind for one key (the FTS5 index is maintained by the
/// `fts_doc` delete trigger). Shared by tombstone and `DerivedWrite::removed`.
pub(crate) fn delete_derived_rows(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<()> {
    // `event_occurrence` keys the object as `event`; every other table as
    // `provider_key`.
    tx.execute(
        "DELETE FROM event_occurrence WHERE scope_key = ?1 AND event = ?2",
        (scope_key, key),
    )
    .map_err(convert::backend)?;
    for table in [
        "fts_doc",
        "mail_index",
        "mail_address",
        "membership",
        "event_index",
        "event_participant",
        // Forward-ready vector data (nothing writes it yet); cleared here so a
        // tombstone/re-index never leaves orphan vectors once it does.
        "embedding",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE scope_key = ?1 AND provider_key = ?2"),
            (scope_key, key),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Counts the structured-index rows stored for one object, for `StoreRead`.
///
/// The `embedding` table (deferred vector data) is cleared by
/// [`delete_derived_rows`] but is not a structured-index row, so it is
/// intentionally not counted here.
pub(crate) fn index_row_counts(
    conn: &Connection,
    scope_key: &str,
    key: &str,
) -> Result<IndexRowCounts> {
    Ok(IndexRowCounts {
        fts: count_for_key(conn, "fts_doc", "provider_key", scope_key, key)?,
        occurrences: count_for_key(conn, "event_occurrence", "event", scope_key, key)?,
        mail_index: count_for_key(conn, "mail_index", "provider_key", scope_key, key)?,
        addresses: count_for_key(conn, "mail_address", "provider_key", scope_key, key)?,
        memberships: count_for_key(conn, "membership", "provider_key", scope_key, key)?,
        event_index: count_for_key(conn, "event_index", "provider_key", scope_key, key)?,
        participants: count_for_key(conn, "event_participant", "provider_key", scope_key, key)?,
    })
}

/// Splits the field-tagged FTS text across the three `fts_doc` columns. `subject`
/// and `location` map by field name; every other field (`body`, and future fields
/// such as attachment text) folds into `body`, so unscoped free text still matches
/// it. Repeated fields are space-joined.
fn fts_columns(fields: &[FtsField]) -> (String, String, String) {
    let mut subject = String::new();
    let mut body = String::new();
    let mut location = String::new();
    for field in fields {
        let target = match field.name.as_str() {
            "subject" => &mut subject,
            "location" => &mut location,
            _ => &mut body,
        };
        if !target.is_empty() {
            target.push(' ');
        }
        target.push_str(&field.text);
    }
    (subject, body, location)
}

/// Replaces each batched object's `mail_address` rows.
fn replace_addresses(tx: &Transaction<'_>, scope_key: &str, rows: &[MailAddressRow]) -> Result<()> {
    let keys = rows.iter().map(|r| r.key.as_str());
    delete_junction_keys(tx, scope_key, "mail_address", keys)?;
    for row in rows {
        tx.execute(
            "INSERT INTO mail_address (scope_key, provider_key, field, addr, name)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key, field, addr) DO UPDATE SET name = excluded.name",
            (
                scope_key,
                row.key.as_str(),
                convert::address_field_text(row.field),
                row.addr.as_str(),
                row.name.as_deref(),
            ),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Replaces each batched object's `membership` rows.
fn replace_memberships(
    tx: &Transaction<'_>,
    scope_key: &str,
    rows: &[MembershipRow],
) -> Result<()> {
    let keys = rows.iter().map(|r| r.key.as_str());
    delete_junction_keys(tx, scope_key, "membership", keys)?;
    for row in rows {
        tx.execute(
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, provider_key, kind, value) DO NOTHING",
            (
                scope_key,
                row.key.as_str(),
                convert::membership_kind_text(row.kind),
                row.value.as_str(),
            ),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Replaces each batched object's `event_participant` rows.
fn replace_participants(
    tx: &Transaction<'_>,
    scope_key: &str,
    rows: &[EventParticipantRow],
) -> Result<()> {
    let keys = rows.iter().map(|r| r.key.as_str());
    delete_junction_keys(tx, scope_key, "event_participant", keys)?;
    for row in rows {
        tx.execute(
            "INSERT INTO event_participant (scope_key, provider_key, role, addr, partstat)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key, role, addr) DO UPDATE SET partstat = excluded.partstat",
            (
                scope_key,
                row.key.as_str(),
                convert::participant_field_text(row.field),
                row.addr.as_str(),
                row.partstat.as_str(),
            ),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Deletes a junction table's rows for every distinct object key in a batch, so
/// the following inserts replace (not append to) each object's set.
fn delete_junction_keys<'a>(
    tx: &Transaction<'_>,
    scope_key: &str,
    table: &str,
    keys: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let unique: HashSet<&str> = keys.collect();
    for key in unique {
        tx.execute(
            &format!("DELETE FROM {table} WHERE scope_key = ?1 AND provider_key = ?2"),
            (scope_key, key),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Counts rows in `table` for one `(scope, key)`, keying the object on `column`.
fn count_for_key(
    conn: &Connection,
    table: &str,
    column: &str,
    scope_key: &str,
    key: &str,
) -> Result<usize> {
    let count: i64 = conn
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE scope_key = ?1 AND {column} = ?2"),
            (scope_key, key),
            |r| r.get(0),
        )
        .map_err(convert::backend)?;
    usize::try_from(count).map_err(convert::backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::ids::{AccountId, ProviderKey};
    use engine_core::search_index::FtsRow;
    use engine_core::sync::{JmapDataType, SyncScope};
    use engine_core::time::UtcDateTime;
    use engine_store::{FtsField, OccurrenceRow, TzdataVersion, WorkerId};
    use rusqlite::Connection;

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
            "c1",
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
            "c2",
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
            "c1",
        )
        .unwrap();
        apply(
            &mut conn,
            &key,
            token,
            &delta_change("e1"),
            &derived,
            &[],
            "c1",
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
            "c1",
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
            "c1",
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
            "c1",
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
}
