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
//! a [`backfill`](crate::backfill) instead: it fills the new shape from `object`, which already
//! holds the normalized record, by running the engine's own projection over it.
//!
//! Postgres will use the same discipline later via a `schema_migrations` table
//! (it has no `user_version`); the migration SQL stays per-store because the
//! dialects differ, while the portable query layer lives in `engine-search`.

use engine_store::{Result, StoreError};
use rusqlite::{Connection, Transaction};

use crate::{backfill, convert::backend, schema};

/// One migration step: its DDL, and optionally a data move that must commit with it.
///
/// A backfill exists for one reason — a new table whose contents are a *function* of what the
/// store already holds. Expressing that function twice (once in the engine, once as SQL over the
/// stored JSON) is how the copy and the original drift, so the step runs the engine's own
/// projection over the rows instead. It sees the same transaction as the DDL, so a database is
/// never at the new version with the new table empty.
struct Migration {
    sql: &'static str,
    backfill: Option<fn(&Transaction<'_>) -> Result<()>>,
}

impl Migration {
    /// A step that is only DDL.
    const fn sql(sql: &'static str) -> Self {
        Self {
            sql,
            backfill: None,
        }
    }

    /// A step whose DDL is followed, in the same transaction, by a data move.
    const fn with_backfill(
        sql: &'static str,
        backfill: fn(&Transaction<'_>) -> Result<()>,
    ) -> Self {
        Self {
            sql,
            backfill: Some(backfill),
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
    Migration::with_backfill(schema::V9, backfill::messages_from_objects),
    Migration::sql(schema::V10),
];

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
fn run(conn: &mut Connection, migrations: &[Migration]) -> Result<()> {
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
        if let Some(backfill) = step.backfill {
            backfill(&tx)?;
        }
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
        assert_eq!(table_count(&conn, "contact_state"), 1);
        assert_eq!(table_count(&conn, "recipient_observation"), 1);
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
        run(
            &mut conn,
            &[Migration::sql("CREATE TABLE a (x TEXT) STRICT;")],
        )
        .unwrap();
        assert_eq!(version(&conn), 1);
        assert_eq!(table_count(&conn, "b"), 0);

        // Adding a v2 step applies only the new step to the existing database.
        run(
            &mut conn,
            &[
                Migration::sql("CREATE TABLE a (x TEXT) STRICT;"),
                Migration::sql("CREATE TABLE b (y TEXT) STRICT;"),
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
                Migration::sql("CREATE TABLE a (x TEXT) STRICT;"),
                Migration::sql("CREATE TABLE b (y TEXT) STRICT;"),
            ],
        )
        .unwrap();
        // An older build (one known step) must not touch a v2 database.
        let refused = run(
            &mut conn,
            &[Migration::sql("CREATE TABLE a (x TEXT) STRICT;")],
        );
        assert!(matches!(refused, Err(StoreError::Backend(_))));
        assert_eq!(version(&conn), 2);
    }

    /// The v9 backfill is what keeps a reshaping step from costing the user a re-download, so what
    /// it must do is carry the mail already stored into the new table with the fields a list row
    /// renders — the ones `mail_index` never held and only the payload knew.
    #[test]
    fn the_message_table_is_filled_from_the_mail_already_stored() {
        use engine_core::{
            ids::{MailboxId, MessageId, MessageIdHeader},
            mail::{EmailAddress, Keyword, Message, SystemKeyword},
            membership::Memberships,
        };

        let mut conn = Connection::open_in_memory().unwrap();
        // A store as it stood before this step: everything up to v8, and no `message` table.
        run(&mut conn, &MIGRATIONS[..8]).unwrap();

        let mut message = Message::new(
            MessageId::try_from("m1").unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        );
        message.envelope.subject = Some("Quarterly report".into());
        message.envelope.from = vec![EmailAddress::named("Alice", "alice@example.com")];
        message.envelope.message_id = vec![MessageIdHeader::new("m1@example.com").unwrap()];
        message.received_at = Some("2026-01-02T03:04:05Z".parse().unwrap());
        message.preview = Some("see attached".into());
        message
            .keywords
            .insert(Keyword::system(SystemKeyword::Seen));
        let payload = serde_json::to_string(&message).unwrap();

        conn.execute(
            "INSERT INTO sync_scope (scope_key, account, token) VALUES ('s1', 'acct', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO object (scope_key, provider_key, payload) VALUES ('s1', 'm1', ?1)",
            [&payload],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_index (scope_key, provider_key, date_utc, has_attachment)
             VALUES ('s1', 'm1', '2026-01-02T03:04:05Z', 0)",
            [],
        )
        .unwrap();
        // An object with no mail-index row is not mail (a calendar event, a mailbox): it must not
        // be dragged into the mail table.
        conn.execute(
            "INSERT INTO object (scope_key, provider_key, payload) VALUES ('s1', 'e1', '{}')",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(version(&conn), i64::try_from(MIGRATIONS.len()).unwrap());
        assert_eq!(table_count(&conn, "mail_index"), 0, "v10 retires it");
        let row: (
            String,
            String,
            Option<String>,
            Option<String>,
            i64,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT account, provider_key, subject, from_name, flags, message_id
                   FROM message",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "acct", "the account comes from the row's scope");
        assert_eq!(row.1, "m1");
        assert_eq!(row.2.as_deref(), Some("Quarterly report"));
        assert_eq!(row.3.as_deref(), Some("Alice"));
        assert_eq!(row.4, 1, "$seen, through the engine's own projection");
        assert_eq!(row.5.as_deref(), Some("m1@example.com"));
    }

    #[test]
    fn a_failing_backfill_takes_its_own_ddl_with_it() {
        let mut conn = Connection::open_in_memory().unwrap();
        let failing = run(
            &mut conn,
            &[Migration::with_backfill(
                "CREATE TABLE a (x TEXT) STRICT;",
                |tx| {
                    tx.execute_batch("NOT VALID SQL;").map_err(backend)?;
                    Ok(())
                },
            )],
        );
        assert!(failing.is_err());
        assert_eq!(version(&conn), 0);
        assert_eq!(
            table_count(&conn, "a"),
            0,
            "a version whose data move failed must not be recorded as applied"
        );
    }

    #[test]
    fn a_failing_step_rolls_back_and_leaves_the_version_unchanged() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(
            &mut conn,
            &[Migration::sql("CREATE TABLE a (x TEXT) STRICT;")],
        )
        .unwrap();
        // A v2 step with invalid SQL must not advance the version.
        let failed = run(
            &mut conn,
            &[
                Migration::sql("CREATE TABLE a (x TEXT) STRICT;"),
                Migration::sql("NOT VALID SQL;"),
            ],
        );
        assert!(failed.is_err());
        assert_eq!(version(&conn), 1);
        assert_eq!(table_count(&conn, "a"), 1);
    }
}
