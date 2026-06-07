//! The scope half of the store: claim, apply (delta/snapshot), maintenance,
//! release, cursor read, and the lease-free object reads.
//!
//! These are synchronous functions over a [`Connection`]; the async trait methods
//! in `lib.rs` offload them onto a blocking thread. They mirror the in-memory
//! reference store's semantics exactly so the shared contract suite passes
//! identically: liveness by lease expiry, supremacy by fencing token re-checked
//! inside the write transaction, and a snapshot tombstoning only locally-present
//! keys absent from its id set.

use std::collections::HashSet;

use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_core::time::UtcDateTime;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::Value;

use engine_store::{
    DerivedWrite, FenceToken, PendingReconciliation, Result, StorableObject, StoreError,
    SyncApplied, SyncClaim, SyncLease, WorkerId,
};

use crate::convert;

/// An owned, type-erased projection of a [`SyncUpdate`], built before the work is
/// offloaded to the blocking thread (where the generic object type `T` is gone).
/// Each object is `(provider_key, payload_json)`.
pub(crate) enum OwnedUpdate {
    /// An incremental change set.
    Delta {
        /// Created/updated objects.
        changed: Vec<(String, String)>,
        /// Destroyed provider keys.
        removed: Vec<String>,
    },
    /// A snapshot whose `present` set drives tombstoning.
    Snapshot {
        /// The objects the snapshot carries.
        objects: Vec<(String, String)>,
        /// The complete provider-key set; locally-present keys absent from it are
        /// tombstoned.
        present: Vec<String>,
    },
}

impl OwnedUpdate {
    /// Serializes a borrowed update into the owned form the apply closure moves.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] if an object cannot be serialized.
    pub(crate) fn from_update<T>(update: &SyncUpdate<T>) -> Result<Self>
    where
        T: StorableObject + Serialize,
    {
        Ok(match update {
            SyncUpdate::Delta { changed, removed } => Self::Delta {
                changed: serialize_objects(changed)?,
                removed: removed.iter().map(|k| k.as_str().to_owned()).collect(),
            },
            SyncUpdate::Snapshot { objects, present } => Self::Snapshot {
                objects: serialize_objects(objects)?,
                present: present.iter().map(|k| k.as_str().to_owned()).collect(),
            },
        })
    }
}

/// Serializes each object to `(provider_key, payload_json)`.
fn serialize_objects<T>(objects: &[T]) -> Result<Vec<(String, String)>>
where
    T: StorableObject + Serialize,
{
    objects
        .iter()
        .map(|obj| {
            let payload = serde_json::to_string(obj).map_err(convert::backend)?;
            Ok((obj.provider_key().as_str().to_owned(), payload))
        })
        .collect()
}

/// Acquires the scope lease and returns the current cursor, bumping the fencing
/// generation so any older lease is now stale.
///
/// # Errors
///
/// Returns [`StoreError::ScopeHeld`] if a live lease exists, or
/// [`StoreError::Backend`] on a backend failure.
pub(crate) fn claim(
    conn: &mut Connection,
    account: AccountId,
    scope: SyncScope,
    scope_key: &str,
    owner: WorkerId,
    now: UtcDateTime,
    expiry: UtcDateTime,
) -> Result<SyncClaim> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let row = tx
        .query_row(
            "SELECT token, lease_expiry, cursor FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(convert::backend)?;
    let (current, lease_expiry, cursor) = match row {
        Some((token, expiry, cursor)) => (convert::generation_from_i64(token)?, expiry, cursor),
        None => (0, None, None),
    };
    if convert::is_live(convert::parse_opt_instant(lease_expiry)?, now) {
        return Err(StoreError::ScopeHeld);
    }

    let token = FenceToken::from_generation(current).bump();
    tx.execute(
        "INSERT INTO sync_scope (scope_key, account, token, lease_expiry, cursor)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(scope_key)
         DO UPDATE SET token = excluded.token, lease_expiry = excluded.lease_expiry",
        (
            scope_key,
            account.as_str(),
            convert::generation_to_i64(token.get())?,
            convert::instant_to_text(expiry),
            cursor.as_deref(),
        ),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;

    let state = cursor.map(SyncState::new);
    Ok(SyncClaim::new(
        SyncLease::new(account, scope, token, owner, expiry),
        state,
    ))
}

/// Reads a scope's cursor without taking a lease.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn load_state(conn: &Connection, scope_key: &str) -> Result<Option<SyncState>> {
    let cursor: Option<Option<String>> = conn
        .query_row(
            "SELECT cursor FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(convert::backend)?;
    Ok(cursor.flatten().map(SyncState::new))
}

/// Commits one scope's atomic apply, gated by the lease token.
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the token is no longer current, or
/// [`StoreError::Backend`] on a backend failure.
pub(crate) fn apply(
    conn: &mut Connection,
    scope_key: &str,
    token: u64,
    update: &OwnedUpdate,
    derived: &DerivedWrite,
    reconcile: &[PendingReconciliation],
    next_state: &str,
) -> Result<SyncApplied> {
    let tx = conn.transaction().map_err(convert::backend)?;
    check_token(&tx, scope_key, token)?;

    let mut applied = SyncApplied::default();
    match update {
        OwnedUpdate::Delta { changed, removed } => {
            for (key, payload) in changed {
                upsert_object(&tx, scope_key, key, payload)?;
                applied.upserted += 1;
            }
            for key in removed {
                if tombstone(&tx, scope_key, key)? {
                    applied.tombstoned += 1;
                }
            }
        }
        OwnedUpdate::Snapshot { objects, present } => {
            for (key, payload) in objects {
                upsert_object(&tx, scope_key, key, payload)?;
                applied.upserted += 1;
            }
            let present: HashSet<&str> = present.iter().map(String::as_str).collect();
            for key in existing_keys(&tx, scope_key)? {
                if !present.contains(key.as_str()) {
                    tombstone(&tx, scope_key, &key)?;
                    applied.tombstoned += 1;
                }
            }
        }
    }

    apply_derived(&tx, scope_key, derived)?;

    for rec in reconcile {
        if reconcile_op(&tx, rec)? {
            applied.reconciled += 1;
        }
    }

    tx.execute(
        "UPDATE sync_scope SET cursor = ?1 WHERE scope_key = ?2",
        (next_state, scope_key),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(applied)
}

/// Writes only derived rows, gated by the same scope lease as sync.
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the token is no longer current, or
/// [`StoreError::Backend`] on a backend failure.
pub(crate) fn maintenance(
    conn: &mut Connection,
    scope_key: &str,
    token: u64,
    derived: &DerivedWrite,
) -> Result<()> {
    let tx = conn.transaction().map_err(convert::backend)?;
    check_token(&tx, scope_key, token)?;
    apply_derived(&tx, scope_key, derived)?;
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// Clears the lease, but only for the current holder (token must match).
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn release(conn: &mut Connection, scope_key: &str, token: u64) -> Result<()> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let current: Option<i64> = tx
        .query_row(
            "SELECT token FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    if current.is_some_and(|t| convert::generation_from_i64(t).is_ok_and(|g| g == token)) {
        tx.execute(
            "UPDATE sync_scope SET lease_expiry = NULL WHERE scope_key = ?1",
            [scope_key],
        )
        .map_err(convert::backend)?;
    }
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// The provider keys of live objects in a scope, ordered lexicographically (the
/// reference store sorts the same way; SQLite's default `BINARY` collation
/// matches `ProviderKey`'s `Ord`).
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn object_keys(conn: &Connection, scope_key: &str) -> Result<Vec<ProviderKey>> {
    let mut stmt = conn
        .prepare("SELECT provider_key FROM object WHERE scope_key = ?1 ORDER BY provider_key")
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([scope_key], |r| r.get::<_, String>(0))
        .map_err(convert::backend)?;
    let mut keys = Vec::new();
    for row in rows {
        let raw = row.map_err(convert::backend)?;
        keys.push(ProviderKey::new(raw).map_err(convert::backend)?);
    }
    Ok(keys)
}

/// The stored payload for one object, or `None` if absent/tombstoned.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn object_payload(
    conn: &Connection,
    scope_key: &str,
    provider_key: &str,
) -> Result<Option<Value>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM object WHERE scope_key = ?1 AND provider_key = ?2",
            (scope_key, provider_key),
            |r| r.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    match payload {
        Some(text) => Ok(Some(serde_json::from_str(&text).map_err(convert::backend)?)),
        None => Ok(None),
    }
}

/// Fails with [`StoreError::StaleLease`] unless the scope's stored generation
/// equals `token` (the fencing check, inside the write transaction).
fn check_token(tx: &Transaction<'_>, scope_key: &str, token: u64) -> Result<()> {
    let current: Option<i64> = tx
        .query_row(
            "SELECT token FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |r| r.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    match current {
        Some(stored) if convert::generation_from_i64(stored)? == token => Ok(()),
        _ => Err(StoreError::StaleLease),
    }
}

/// Upserts one object's payload, keyed by its provider key.
fn upsert_object(tx: &Transaction<'_>, scope_key: &str, key: &str, payload: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO object (scope_key, provider_key, payload) VALUES (?1, ?2, ?3)
         ON CONFLICT(scope_key, provider_key) DO UPDATE SET payload = excluded.payload",
        (scope_key, key, payload),
    )
    .map_err(convert::backend)?;
    Ok(())
}

/// Removes an object and the derived rows keyed by it. Returns whether the object
/// existed (so snapshot/delta tombstone counts match the reference store).
fn tombstone(tx: &Transaction<'_>, scope_key: &str, key: &str) -> Result<bool> {
    let existed = tx
        .execute(
            "DELETE FROM object WHERE scope_key = ?1 AND provider_key = ?2",
            (scope_key, key),
        )
        .map_err(convert::backend)?
        > 0;
    tx.execute(
        "DELETE FROM fts_doc WHERE scope_key = ?1 AND provider_key = ?2",
        (scope_key, key),
    )
    .map_err(convert::backend)?;
    tx.execute(
        "DELETE FROM event_occurrence WHERE scope_key = ?1 AND event = ?2",
        (scope_key, key),
    )
    .map_err(convert::backend)?;
    Ok(existed)
}

/// All live object keys in a scope (used to compute snapshot tombstones).
fn existing_keys(tx: &Transaction<'_>, scope_key: &str) -> Result<Vec<String>> {
    let mut stmt = tx
        .prepare("SELECT provider_key FROM object WHERE scope_key = ?1")
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([scope_key], |r| r.get::<_, String>(0))
        .map_err(convert::backend)?;
    rows.collect::<rusqlite::Result<Vec<String>>>()
        .map_err(convert::backend)
}

/// Applies the precomputed derived rows: upsert FTS docs and occurrences, then
/// clear derived rows for explicitly removed keys (recurrence/tz invalidation).
fn apply_derived(tx: &Transaction<'_>, scope_key: &str, derived: &DerivedWrite) -> Result<()> {
    for row in &derived.fts {
        let fields = serde_json::to_string(&row.fields).map_err(convert::backend)?;
        tx.execute(
            "INSERT INTO fts_doc (scope_key, provider_key, fields) VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key, provider_key) DO UPDATE SET fields = excluded.fields",
            (scope_key, row.key.as_str(), fields),
        )
        .map_err(convert::backend)?;
    }
    for occ in &derived.occurrences {
        let recurrence_id = occ
            .recurrence_id
            .map(convert::instant_to_text)
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO event_occurrence (scope_key, event, start_utc, end_utc, recurrence_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope_key, event, start_utc, recurrence_id)
             DO UPDATE SET end_utc = excluded.end_utc",
            (
                scope_key,
                occ.event.as_str(),
                convert::instant_to_text(occ.start),
                convert::instant_to_text(occ.end),
                recurrence_id,
            ),
        )
        .map_err(convert::backend)?;
    }
    for key in &derived.removed {
        tx.execute(
            "DELETE FROM fts_doc WHERE scope_key = ?1 AND provider_key = ?2",
            (scope_key, key.as_str()),
        )
        .map_err(convert::backend)?;
        tx.execute(
            "DELETE FROM event_occurrence WHERE scope_key = ?1 AND event = ?2",
            (scope_key, key.as_str()),
        )
        .map_err(convert::backend)?;
    }
    Ok(())
}

/// Re-validates a planned reconciliation inside the transaction: if the op is
/// still in its expected state, resolve it to `Succeeded`; otherwise skip it (the
/// incoming object is stored normally regardless). Returns whether it applied.
fn reconcile_op(tx: &Transaction<'_>, rec: &PendingReconciliation) -> Result<bool> {
    let id = convert::op_id_to_i64(rec.op)?;
    let current: Option<String> = tx
        .query_row("SELECT state FROM pending_op WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(convert::backend)?;
    let matches = match current {
        Some(state) => convert::parse_state(&state)? == rec.expected,
        None => false,
    };
    if matches {
        tx.execute(
            "UPDATE pending_op SET state = 'Succeeded', lease_expiry = NULL WHERE id = ?1",
            [id],
        )
        .map_err(convert::backend)?;
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::sync::JmapDataType;
    use engine_store::{FtsField, FtsRow, OccurrenceRow};

    fn instant(text: &str) -> UtcDateTime {
        text.parse().expect("valid instant")
    }

    fn pk(value: &str) -> ProviderKey {
        ProviderKey::new(value).expect("valid key")
    }

    fn open() -> (Connection, String) {
        let mut conn = Connection::open_in_memory().expect("open");
        crate::migrations::migrate(&mut conn).expect("schema");
        (conn, convert::scope_key(&events_scope()))
    }

    fn events_scope() -> SyncScope {
        SyncScope::JmapType {
            account: AccountId::try_from("a").expect("valid account"),
            data_type: JmapDataType::CalendarEvent,
        }
    }

    /// Claims the scope and returns the current fencing token.
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
        }
    }

    #[test]
    fn tombstoning_an_object_cascades_to_its_derived_rows() {
        let (mut conn, key) = open();
        let token = claim_token(&mut conn, &key);

        let mut derived = DerivedWrite::empty();
        derived.fts.push(FtsRow::new(
            pk("e1"),
            vec![FtsField::new("summary", "standup")],
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
        assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 1);

        // A delta removal tombstones the object and its derived rows together.
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
        // The store keys occurrences by (event, start, recurrence_id), so a replay
        // of the same batch is idempotent rather than additive.
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
            .push(FtsRow::new(pk("e1"), vec![FtsField::new("summary", "x")]));
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

        // `DerivedWrite.removed` clears derived rows (e.g. recurrence-rule change)
        // without tombstoning the object itself.
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
        // The base row (recurrence_id '') and the override are distinct rows.
        assert_eq!(count(&conn, "SELECT count(*) FROM event_occurrence"), 2);
    }
}
