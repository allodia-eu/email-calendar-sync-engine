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
//! Postgres will use the same discipline later via a `schema_migrations` table
//! (it has no `user_version`); the migration SQL stays per-store because the
//! dialects differ, while the portable query layer lives in `engine-search`.

use engine_store::{Result, StoreError};
use rusqlite::Connection;

use crate::convert::backend;
use crate::schema;

/// The ordered migration steps. Index `i` is schema version `i + 1`; the stored
/// `user_version` is the count applied. **Append only** — never edit or reorder a
/// shipped step.
const MIGRATIONS: &[&str] = &[schema::V1, schema::V2, schema::V3, schema::V4];

/// Brings `conn` up to the latest schema version.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] if a step fails or the database is newer than
/// this build understands.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    run(conn, MIGRATIONS)
}

/// The version-driven runner, parameterized over the step list for testing.
fn run(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
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
        tx.execute_batch(step).map_err(backend)?;
        // `user_version` is a transaction-safe header write, so the step and the
        // version bump commit together; it cannot be bound, so format the checked
        // integer in directly.
        tx.execute_batch(&format!("PRAGMA user_version = {version};"))
            .map_err(backend)?;
        tx.commit().map_err(backend)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    fn table_count(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn fresh_database_applies_every_step_and_records_the_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), i64::try_from(MIGRATIONS.len()).unwrap());
        // The v1 tables exist.
        assert_eq!(table_count(&conn, "object"), 1);
        assert_eq!(table_count(&conn, "pending_op"), 1);
    }

    #[test]
    fn rerunning_is_a_noop() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let after_first = version(&conn);
        // A second run applies nothing and does not error on the existing tables.
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), after_first);
    }

    #[test]
    fn pending_steps_apply_incrementally_to_an_existing_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Start at v1.
        run(&mut conn, &["CREATE TABLE a (x TEXT) STRICT;"]).unwrap();
        assert_eq!(version(&conn), 1);
        assert_eq!(table_count(&conn, "b"), 0);

        // Adding a v2 step applies only the new step to the existing database.
        run(
            &mut conn,
            &[
                "CREATE TABLE a (x TEXT) STRICT;",
                "CREATE TABLE b (y TEXT) STRICT;",
            ],
        )
        .unwrap();
        assert_eq!(version(&conn), 2);
        assert_eq!(table_count(&conn, "a"), 1);
        assert_eq!(table_count(&conn, "b"), 1);
    }

    #[test]
    fn a_database_newer_than_the_build_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(
            &mut conn,
            &[
                "CREATE TABLE a (x TEXT) STRICT;",
                "CREATE TABLE b (y TEXT) STRICT;",
            ],
        )
        .unwrap();
        // An older build (one known step) must not touch a v2 database.
        let refused = run(&mut conn, &["CREATE TABLE a (x TEXT) STRICT;"]);
        assert!(matches!(refused, Err(StoreError::Backend(_))));
        assert_eq!(version(&conn), 2);
    }

    #[test]
    fn a_failing_step_rolls_back_and_leaves_the_version_unchanged() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &["CREATE TABLE a (x TEXT) STRICT;"]).unwrap();
        // A v2 step with invalid SQL must not advance the version.
        let failed = run(
            &mut conn,
            &["CREATE TABLE a (x TEXT) STRICT;", "NOT VALID SQL;"],
        );
        assert!(failed.is_err());
        assert_eq!(version(&conn), 1);
        assert_eq!(table_count(&conn, "a"), 1);
    }
}
