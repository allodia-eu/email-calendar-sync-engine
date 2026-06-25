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

use crate::{convert, derived_ops};

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
    next_state: Option<&str>,
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

    derived_ops::apply_derived(&tx, scope_key, derived)?;

    for rec in reconcile {
        if reconcile_op(&tx, rec)? {
            applied.reconciled += 1;
        }
    }

    // A streaming page (`next_state == None`) leaves the cursor unchanged so a
    // crash mid-stream re-syncs from the prior cursor rather than skipping pages.
    if let Some(next_state) = next_state {
        tx.execute(
            "UPDATE sync_scope SET cursor = ?1 WHERE scope_key = ?2",
            (next_state, scope_key),
        )
        .map_err(convert::backend)?;
    }
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
    derived_ops::apply_derived(&tx, scope_key, derived)?;
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

/// Every scope the store knows for `account`, decoded from its stored JSON
/// `scope_key` (the canonical [`SyncScope`] form `convert::scope_key` writes) and
/// sorted ascending — so a per-account search can enumerate scopes rather than
/// hard-code which a provider uses.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure or a corrupt scope key.
pub(crate) fn account_scopes(conn: &Connection, account: &AccountId) -> Result<Vec<SyncScope>> {
    let mut stmt = conn
        .prepare("SELECT scope_key FROM sync_scope WHERE account = ?1")
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map([account.as_str()], |r| r.get::<_, String>(0))
        .map_err(convert::backend)?;
    let mut scopes = Vec::new();
    for row in rows {
        let key = row.map_err(convert::backend)?;
        scopes.push(serde_json::from_str::<SyncScope>(&key).map_err(convert::backend)?);
    }
    scopes.sort();
    Ok(scopes)
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
    derived_ops::delete_derived_rows(tx, scope_key, key)?;
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
