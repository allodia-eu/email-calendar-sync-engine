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
mod tests {
    use super::*;

    /// The version this build expects — the number of steps it knows.
    fn expected_version() -> u32 {
        u32::try_from(MIGRATIONS.len()).unwrap()
    }

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The normalized payload a v9 store held for a message, as the engine writes it.
    fn stored_payload(key: &str, owned: &str, references: &[&str]) -> String {
        use engine_core::{
            ids::{MailboxId, MessageId, MessageIdHeader},
            mail::{MailContent, Message},
            membership::Memberships,
        };
        let mut message = Message::new(
            MessageId::try_from(key).unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        );
        message.envelope.message_id = vec![MessageIdHeader::new(owned).unwrap()];
        message.envelope.references = references
            .iter()
            .map(|id| MessageIdHeader::new(*id).unwrap())
            .collect();
        serde_json::to_string(&MailContent::from(&message)).unwrap()
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

    /// v11 adds the column that separates "no photo here" from "never asked", and every
    /// photo an existing store already cached was written before that column existed.
    /// Those rows must keep reading as photos: defaulted the other way, a user who has
    /// used the app would find every cached picture reinterpreted as an absence and
    /// remembered as one.
    #[test]
    fn photos_cached_before_v11_still_read_as_photos_after_it() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &MIGRATIONS[..10]).unwrap();
        assert_eq!(version(&conn), 10);
        // Written the way a v10 build wrote it — the `missing` column does not exist yet.
        conn.execute(
            "INSERT INTO contact_photo
             (account, contact, resource, fingerprint, content_hash, media_type, fetched_at)
             VALUES ('a', 'c', 'photo', 'etag:v1', 'deadbeef', 'image/jpeg', '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), i64::from(expected_version()));
        let (hash, missing): (String, i64) = conn
            .query_row(
                "SELECT content_hash, missing FROM contact_photo WHERE contact = 'c'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash, "deadbeef", "the cached photo survives the migration");
        assert_eq!(missing, 0, "an existing photo is not an absence");
    }

    /// v9 leaves a message row carrying both halves of what a list and a write need, and takes
    /// `mail_index` with it. Asserted on the schema a *fresh* store ends up with, because that is
    /// now the only way any store reaches this version.
    #[test]
    fn the_message_table_carries_a_messages_whole_mutable_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();

        assert_eq!(table_count(&conn, "mail_index"), 0, "v9 retires it");

        let mut columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('message')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(std::result::Result::unwrap)
            .collect();
        columns.sort();
        assert_eq!(
            columns,
            vec![
                "account",
                "change_key",
                "date_utc",
                "etag",
                "flags",
                "from_addr",
                "from_name",
                "has_attachment",
                "last_modified",
                "message_id",
                "mod_seq",
                "preview",
                "provider_key",
                "scope_key",
                "size_octets",
                "subject",
                "thread_id",
            ],
            "the row is what a list shows plus the state that moves without the message's bytes, \
             plus how big the provider says it is so a size cap can ask in SQL; `schedule_tag` \
             is CalDAV scheduling state and has no place on a message"
        );
    }

    /// Opening a store that is behind reports the pair a support answer needs: what it was, and
    /// what it is now.
    /// The v10 step fills the message-id graph from the payloads already stored, so a store that
    /// upgrades is not left threading nothing.
    ///
    /// The failure this pins is silent: an empty graph reads as "no message shares an id with any
    /// other", so every reply after the upgrade would open a conversation of its own and the
    /// mailbox would look fine until someone counted the rows.
    #[test]
    fn the_v10_step_fills_the_graph_from_the_payloads_already_stored() {
        let mut conn = Connection::open_in_memory().unwrap();
        // Bring the database up to v9 — everything before the graph existed.
        run(&mut conn, &MIGRATIONS[..9]).unwrap();
        assert_eq!(version(&conn), 9);

        // A stored reply and its original, as v9 held them: a payload plus a message row. The
        // payload goes through the real projection, so a shape change breaks this test rather
        // than silently making the backfill skip every row.
        for (key, owned, references) in [("m1", "a@h", &[][..]), ("m2", "b@h", &["a@h"][..])] {
            conn.execute(
                "INSERT INTO object (scope_key, provider_key, payload) VALUES ('s1', ?1, ?2)",
                (key, stored_payload(key, owned, references)),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (scope_key, provider_key, account, flags, has_attachment)
                 VALUES ('s1', ?1, 'acct', 0, 0)",
                [key],
            )
            .unwrap();
        }

        run(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(version(&conn), i64::from(expected_version()));

        let rows: i64 = conn
            .query_row("SELECT count(*) FROM msgid_ref", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 3, "two owned ids and one reference");
        let owned: i64 = conn
            .query_row("SELECT count(*) FROM msgid_ref WHERE owned = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            owned, 2,
            "the reference is not owned, so it cannot name a thread"
        );
        // The account rides the row, because the lookup that matters is account-wide.
        let scoped: i64 = conn
            .query_row(
                "SELECT count(*) FROM msgid_ref WHERE account = 'acct' AND msgid = 'a@h'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scoped, 2, "both messages touch the original's id");
    }

    /// A payload that will not decode is skipped, not fatal: it is already unreadable by every
    /// other path, and failing here would leave a store that cannot open at all.
    #[test]
    fn an_undecodable_payload_does_not_fail_the_upgrade() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &MIGRATIONS[..9]).unwrap();
        conn.execute(
            "INSERT INTO object (scope_key, provider_key, payload) VALUES ('s1', 'bad', 'not json')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (scope_key, provider_key, account, flags, has_attachment)
             VALUES ('s1', 'bad', 'acct', 0, 0)",
            [],
        )
        .unwrap();

        run(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(version(&conn), i64::from(expected_version()));
    }

    #[test]
    fn migrating_reports_the_version_it_moved_from() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A store as an older build left it.
        run(&mut conn, &MIGRATIONS[..4]).unwrap();

        let status = migrate(&mut conn).unwrap();

        assert_eq!(status.migrated_from, Some(4), "where it came from");
        assert_eq!(status.version, expected_version(), "where it landed");
        assert_eq!(status.expected, expected_version());
        assert!(status.migrated());
    }

    /// A fresh store had no version to move from, and an already-current one did not move.
    ///
    /// Both report no migration, so a host that logs the pair says nothing on an ordinary
    /// launch — which is what keeps the line meaningful when it does appear.
    #[test]
    fn a_fresh_or_current_store_reports_no_migration() {
        let mut conn = Connection::open_in_memory().unwrap();

        let fresh = migrate(&mut conn).unwrap();
        assert_eq!(fresh.migrated_from, None, "nothing to migrate from");
        assert_eq!(fresh.version, expected_version());

        let reopened = migrate(&mut conn).unwrap();
        assert_eq!(reopened.migrated_from, None, "already current");
        assert_eq!(reopened.version, expected_version());
    }
}
