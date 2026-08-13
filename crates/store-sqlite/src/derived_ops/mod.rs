//! Writing, clearing, and counting the precomputed derived rows.
//!
//! The store is mechanical: these functions persist the [`DerivedWrite`] the
//! engine computed (`store-and-sync.md`), they never derive anything. Full-text
//! text maps onto the `fts_doc` external-content columns (the FTS5 index follows
//! via triggers); scalar rows upsert; junction rows *replace* per object so a
//! re-projection drops stale rows and a replay is idempotent; `removed` and
//! tombstoning clear every kind together.

use std::collections::HashSet;

use engine_core::{
    calendar::ParticipationStatus,
    ids::{ProviderKey, ThreadId},
    search_index::{EventParticipantRow, FtsField, MailAddressRow, MembershipRow},
    time::Horizon,
};
use engine_store::{
    DerivedWrite, IndexRowCounts, MailIndexEntry, OccurrenceRow, Result, TzdataVersion,
};
use rusqlite::{Connection, Transaction};

use crate::{convert, sql};

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
    for key in &derived.reset_occurrences {
        delete_occurrences(tx, scope_key, key.as_str())?;
    }
    for row in &derived.fts {
        let (subject, body, location) = fts_columns(&row.fields);
        sql::execute(
            tx,
            "INSERT INTO fts_doc (scope_key, provider_key, subject, body, location)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, provider_key) DO UPDATE SET
                 subject = excluded.subject, body = excluded.body, location = excluded.location",
            (scope_key, row.key.as_str(), subject, body, location),
        )?;
    }
    for occ in &derived.occurrences {
        let recurrence_id = occ
            .recurrence_id
            .map(convert::instant_to_text)
            .unwrap_or_default();
        sql::execute(
            tx,
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
        )?;
    }
    for row in &derived.mail_index {
        sql::execute(
            tx,
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
        )?;
    }
    for row in &derived.event_index {
        sql::execute(
            tx,
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
        )?;
    }
    replace_addresses(tx, scope_key, &derived.addresses)?;
    replace_memberships(tx, scope_key, &derived.memberships)?;
    replace_participants(tx, scope_key, &derived.participants)?;
    Ok(())
}

/// Clears one event's occurrence rows, leaving its other derived rows alone — the targeted
/// reset a re-derived event needs (`DerivedWrite::reset_occurrences`).
pub(crate) fn delete_occurrences(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<()> {
    sql::execute(
        tx,
        "DELETE FROM event_occurrence WHERE scope_key = ?1 AND event = ?2",
        (scope_key, key),
    )?;
    Ok(())
}

/// Removes every derived row kind for one key (the FTS5 index is maintained by the
/// `fts_doc` delete trigger). Shared by tombstone and `DerivedWrite::removed`.
pub(crate) fn delete_derived_rows(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<()> {
    // `event_occurrence` keys the object as `event`; every other table as
    // `provider_key`.
    sql::execute(
        tx,
        "DELETE FROM event_occurrence WHERE scope_key = ?1 AND event = ?2",
        (scope_key, key),
    )?;
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
        sql::execute(
            tx,
            &format!("DELETE FROM {table} WHERE scope_key = ?1 AND provider_key = ?2"),
            (scope_key, key),
        )?;
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

/// The scalar mail-index entries for a scope's live mail objects — each object's
/// `(provider key, date, thread)`, without the payload. Backs `StoreRead::scope_mail_index`:
/// a caller ranks by date (a newest-N window) or gathers a thread's members, then decodes
/// only the chosen few. `mail_index` is cleared on tombstone by [`delete_derived_rows`], so
/// its rows are exactly the live mail objects — no join with `object` is needed.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure or
/// a corrupt stored key/date.
pub(crate) fn scope_mail_index(conn: &Connection, scope_key: &str) -> Result<Vec<MailIndexEntry>> {
    let rows: Vec<(String, Option<String>, Option<String>)> = sql::query_all(
        conn,
        "SELECT provider_key, date_utc, thread_id FROM mail_index WHERE scope_key = ?1",
        [scope_key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut entries = Vec::with_capacity(rows.len());
    for (key, date, thread) in rows {
        let key = ProviderKey::new(key).map_err(convert::backend)?;
        let date = convert::parse_opt_instant(date)?;
        let thread = thread
            .map(|raw| ThreadId::try_from(raw.as_str()))
            .transpose()
            .map_err(convert::backend)?;
        entries.push((key, date, thread));
    }
    Ok(entries)
}

/// The provider keys in a scope filed under any of `threads`, ascending and without
/// repeats. Backs `StoreRead::scope_thread_keys` — expanding a conversation.
///
/// One seek per thread rather than a single statement with an `IN` list: the list's
/// length changes with the window, so one statement would compile a new SQL string per
/// call and defeat the statement cache. Each seek here is the same cached statement
/// against `mail_index_thread`, which carries `provider_key` alongside the thread and
/// so answers without reading the table.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend
/// failure or a corrupt stored key.
pub(crate) fn scope_thread_keys(
    conn: &Connection,
    scope_key: &str,
    threads: &[ThreadId],
) -> Result<Vec<ProviderKey>> {
    let mut raw: Vec<String> = Vec::new();
    for thread in threads {
        raw.extend(sql::query_all(
            conn,
            "SELECT provider_key FROM mail_index WHERE scope_key = ?1 AND thread_id = ?2",
            (scope_key, thread.as_str()),
            |r| r.get::<_, String>(0),
        )?);
    }
    raw.sort_unstable();
    raw.dedup();
    raw.into_iter()
        .map(|key| ProviderKey::new(key).map_err(convert::backend))
        .collect()
}

/// The occurrences in a scope that overlap `window`, ascending by `(start, end, event)`.
/// Backs `StoreRead::scope_occurrences` — the range read a calendar grid pages over.
///
/// The predicate is the half-open overlap `start < window.end AND end > window.start`,
/// which the `event_occurrence_range` index on `(scope_key, start_utc, end_utc)` serves as
/// a range scan on its leading columns. A **zero-length** occurrence (`start == end`) is
/// the one case that rule gets wrong — `end > window.start` excludes one sitting exactly
/// on the lower bound — so it is admitted as the point `start`, matching
/// [`Horizon::overlaps`](engine_core::time::Horizon::overlaps). Instants are stored as
/// canonical `Z`-suffixed RFC 3339 text, which is fixed-width and lexicographically
/// ordered, so SQLite's text comparison *is* chronological comparison.
///
/// `event_occurrence` is cleared on tombstone by [`delete_derived_rows`], so its rows are
/// exactly the live events' — no join with `object` is needed.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure
/// or a corrupt stored key/instant.
pub(crate) fn scope_occurrences(
    conn: &Connection,
    scope_key: &str,
    window: Horizon,
) -> Result<Vec<OccurrenceRow>> {
    let rows: Vec<(String, String, String, String, String)> = sql::query_all(
        conn,
        "SELECT event, start_utc, end_utc, recurrence_id, tzdata_version
             FROM event_occurrence
             WHERE scope_key = ?1
               AND start_utc < ?3
               AND (end_utc > ?2 OR (end_utc = start_utc AND start_utc >= ?2))
             ORDER BY start_utc, end_utc, event, recurrence_id",
        (
            scope_key,
            convert::instant_to_text(window.start()),
            convert::instant_to_text(window.end()),
        ),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let mut occurrences = Vec::with_capacity(rows.len());
    for (event, start, end, recurrence_id, tzdata) in rows {
        occurrences.push(OccurrenceRow {
            event: ProviderKey::new(event).map_err(convert::backend)?,
            start: convert::parse_instant(&start)?,
            end: convert::parse_instant(&end)?,
            // An unoverridden instance stores the empty string, not NULL (the column is
            // part of the primary key, which cannot be nullable).
            recurrence_id: if recurrence_id.is_empty() {
                None
            } else {
                Some(convert::parse_instant(&recurrence_id)?)
            },
            tzdata_version: TzdataVersion::new(tzdata),
        });
    }
    Ok(occurrences)
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
        sql::execute(
            tx,
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
        )?;
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
        sql::execute(
            tx,
            "INSERT INTO membership (scope_key, provider_key, kind, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, provider_key, kind, value) DO NOTHING",
            (
                scope_key,
                row.key.as_str(),
                convert::membership_kind_text(row.kind),
                row.value.as_str(),
            ),
        )?;
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
        sql::execute(
            tx,
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
        )?;
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
        sql::execute(
            tx,
            &format!("DELETE FROM {table} WHERE scope_key = ?1 AND provider_key = ?2"),
            (scope_key, key),
        )?;
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
    let count: i64 = sql::query_opt(
        conn,
        &format!("SELECT count(*) FROM {table} WHERE scope_key = ?1 AND {column} = ?2"),
        (scope_key, key),
        |r| r.get(0),
    )?
    .unwrap_or_default();
    usize::try_from(count).map_err(convert::backend)
}

#[cfg(test)]
mod tests;
