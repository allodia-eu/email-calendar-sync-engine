//! Forward-only schema migrations, keyed on `PRAGMA user_version`.
//!
//! `user_version` is a free integer in the SQLite database header (no extra
//! table). On open, [`migrate`] reads it, runs every not-yet-applied step in
//! order — each in its own transaction so a step and its version bump commit
//! atomically — and stops. A fresh database is at version 0 and gets every step;
//! an up-to-date database is a no-op.
//!
//! **Forward-only.** There are no down-migrations: the store is a re-derivable
//! cache of provider data, so a reshaping change can drop and rebuild
//! `object`/`fts_doc`/`event_occurrence` (and force a re-sync) rather than copy
//! data forward — only `pending_op` holds non-re-derivable user writes and must
//! be migrated data-preservingly. Opening a database whose version is *newer*
//! than this build knows about is refused rather than silently mishandled.
//!
//! Re-deriving is cheap only when it costs a *local* pass. A step that would otherwise force a
//! re-**sync** — every message downloaded again over the network, which the user watches — carries
//! a [`backfill`](crate::backfill) step instead: it fills the new shape from `object`, which
//! already holds the normalized record, by running the engine's own projection over it.
//!
//! Postgres will use the same discipline later via a `schema_migrations` table
//! (it has no `user_version`); the migration SQL stays per-store because the
//! dialects differ, while the portable query layer lives in `engine-search`.

use engine_store::{Result, SchemaStatus, StoreError};
use rusqlite::{Connection, Transaction};

use crate::{backfill, convert::backend, schema};

/// One migration step: its DDL, and optionally a data move that must land with it.
///
/// A step that adds a table whose contents are a function of what the store already holds needs
/// the move to commit in the same transaction as the DDL, so a database is never at the new
/// version with the new table empty. The move is pinned to its own version rather than borrowing
/// the live write path, which moves on.
struct Migration {
    sql: &'static str,
    fill: Option<fn(&Transaction<'_>) -> Result<()>>,
}

impl Migration {
    /// A step that is only DDL.
    const fn sql(sql: &'static str) -> Self {
        Self { sql, fill: None }
    }

    /// A step whose new shape is filled from what the store already holds, in the same
    /// transaction.
    const fn filled(sql: &'static str, fill: fn(&Transaction<'_>) -> Result<()>) -> Self {
        Self {
            sql,
            fill: Some(fill),
        }
    }
}

/// The ordered migration steps. Index `i` is schema version `i + 1`; the stored
/// `user_version` is the count applied. **Append only** — never edit or reorder a
/// shipped step.
const MIGRATIONS: &[Migration] = &[
    Migration::sql(schema::V1),
    Migration::sql(schema::V2),
    Migration::sql(schema::V3),
    Migration::sql(schema::V4),
    Migration::sql(schema::V5),
    Migration::sql(schema::V6),
    Migration::sql(schema::V7),
    Migration::sql(schema::V8),
    Migration::sql(schema::V9),
    Migration::filled(schema::V10, backfill::msgid_refs),
    Migration::sql(schema::V11),
    Migration::sql(schema::V12),
    Migration::sql(schema::V13),
    Migration::sql(schema::V14),
    Migration::sql(schema::V15),
    Migration::sql(schema::V16),
    Migration::sql(schema::V17),
];

/// Brings `conn` up to the latest schema version.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] if a step fails or the database is newer than
/// this build understands.
pub(crate) fn migrate(conn: &mut Connection) -> Result<SchemaStatus> {
    run(conn, MIGRATIONS)
}

/// Brings `conn` to schema `version` and no further: the shape an older build left a store in.
#[cfg(test)]
pub(crate) fn migrate_to(conn: &mut Connection, version: usize) -> Result<SchemaStatus> {
    run(conn, &MIGRATIONS[..version])
}

/// The version-driven runner, parameterized over the step list for testing.
fn run(conn: &mut Connection, migrations: &[Migration]) -> Result<SchemaStatus> {
    let current: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(backend)?;
    let applied = usize::try_from(current).map_err(backend)?;
    if applied > migrations.len() {
        return Err(StoreError::Backend(format!(
            "database schema version {applied} is newer than this build ({})",
            migrations.len()
        )));
    }
    for (index, step) in migrations.iter().enumerate().skip(applied) {
        let version = i64::try_from(index + 1).map_err(backend)?;
        let tx = conn.transaction().map_err(backend)?;
        tx.execute_batch(step.sql).map_err(backend)?;
        if let Some(fill) = step.fill {
            fill(&tx)?;
        }
        // `user_version` is a transaction-safe header write, so the step and the
        // version bump commit together; it cannot be bound, so format the checked
        // integer in directly.
        tx.execute_batch(&format!("PRAGMA user_version = {version};"))
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
    }
    let expected = u32::try_from(migrations.len()).map_err(backend)?;
    Ok(SchemaStatus {
        version: expected,
        expected,
        // `None` when nothing moved — an already-current store, or a fresh one that had no
        // version to move *from*. A host logs the pair, so "0 → 9" would be noise on every
        // first launch while "7 → 9" is the answer to a support question.
        migrated_from: (applied > 0 && applied < migrations.len())
            .then(|| u32::try_from(applied).unwrap_or(u32::MAX)),
    })
}

#[cfg(test)]
mod tests;
