//! Unit tests for the migration runner and for the steps that move data.

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

/// v14 adds `kind`, and every op an existing store already holds was enqueued
/// before that column existed. Those rows must survive the step (they are user
/// writes, not a re-derivable cache) and must never be *attempted*: nothing records
/// which request type their payload is, so running one would mean guessing whether
/// a months-old row was an archive, a report or a send.
#[test]
fn v14_keeps_the_ops_a_v13_store_held_and_leaves_them_unclassified() {
    let mut conn = Connection::open_in_memory().unwrap();
    // Bring a store up to v13, the shape before the queue columns existed.
    let upto_v13 = &MIGRATIONS[..13];
    run(&mut conn, upto_v13).unwrap();
    assert_eq!(version(&conn), 13);

    conn.execute(
        "INSERT INTO pending_op
             (account, idempotency_key, resource_key, depends_on, payload, state, token,
              lease_expiry)
         VALUES ('acct-1', 'edit:1757942400:7', 'mail:imap:v1:u42@INBOX', '[]',
                 '{\"MoveTo\":{}}', 'Pending', 0, NULL)",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();
    assert_eq!(version(&conn), i64::from(expected_version()));

    // The row is still there, with its payload and idempotency record intact: that
    // row is what stops the same write being replayed.
    let (idempotency, payload, state, kind, attempts): (
        String,
        String,
        String,
        Option<String>,
        i64,
    ) = conn
        .query_row(
            "SELECT idempotency_key, payload, state, kind, attempts FROM pending_op",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(idempotency, "edit:1757942400:7");
    assert_eq!(payload, "{\"MoveTo\":{}}");
    assert_eq!(state, "Pending");
    assert_eq!(
        kind, None,
        "a pre-v14 row cannot be classified after the fact"
    );
    assert_eq!(attempts, 0);
}

/// v15 rebuilds `person` so a nameless person can be stored. The people an existing store holds
/// are copied across rather than dropped: their ids are what a host holds, and the next rebuild
/// keeps an id only for a person it finds in the previous generation.
#[test]
fn v15_keeps_the_people_a_v14_store_held_and_takes_one_without_a_name() {
    let mut conn = Connection::open_in_memory().unwrap();
    run(&mut conn, &MIGRATIONS[..14]).unwrap();
    assert_eq!(version(&conn), 14);
    conn.execute(
        "INSERT INTO person (id, ordinal, display_name, payload) VALUES (7, 0, 'Ada', '{}')",
        [],
    )
    .unwrap();

    migrate(&mut conn).unwrap();
    assert_eq!(version(&conn), i64::from(expected_version()));

    let kept: (i64, i64, Option<String>, String) = conn
        .query_row(
            "SELECT id, ordinal, display_name, payload FROM person",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(kept, (7, 0, Some("Ada".to_owned()), "{}".to_owned()));
    conn.execute(
        "INSERT INTO person (id, ordinal, display_name, payload) VALUES (8, 1, NULL, '{}')",
        [],
    )
    .unwrap();
    let indexed: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'person_display_name'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "the rebuilt table keeps its index");
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

/// v18 rewrites every instant a store already holds into the form that sorts as text, including
/// in a table keyed on one: an occurrence re-derived afterwards meets its own row, not a second
/// spelling of its start beside it.
#[test]
fn v18_rewrites_stored_instants_into_the_form_that_sorts() {
    let mut conn = Connection::open_in_memory().unwrap();
    run(&mut conn, &MIGRATIONS[..17]).unwrap();
    conn.execute_batch(
        "INSERT INTO event_occurrence
             (scope_key, event, start_utc, end_utc, recurrence_id, tzdata_version)
         VALUES ('s', 'e', '2026-10-06T10:00:00Z', '2026-10-06T10:30:00.5Z', '', '2026a');
         INSERT INTO pending_op
             (account, idempotency_key, resource_key, depends_on, payload, state, token,
              settled_at)
         VALUES ('a', 'k', 'r', '[]', '{}', 'Succeeded', 0, '2026-10-06T10:00:00.12Z');",
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let (start, end, recurrence): (String, String, String) = conn
        .query_row(
            "SELECT start_utc, end_utc, recurrence_id FROM event_occurrence",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(start, "2026-10-06T10:00:00.000000000Z");
    assert_eq!(end, "2026-10-06T10:30:00.500000000Z");
    assert_eq!(
        recurrence, "",
        "an unoverridden instance keeps its empty marker"
    );
    let settled: String = conn
        .query_row("SELECT settled_at FROM pending_op", [], |r| r.get(0))
        .unwrap();
    assert_eq!(settled, "2026-10-06T10:00:00.120000000Z");

    // Re-deriving the occurrence writes the same start, so it meets its own row.
    conn.execute(
        "INSERT INTO event_occurrence
             (scope_key, event, start_utc, end_utc, recurrence_id, tzdata_version)
         VALUES ('s', 'e', ?1, ?2, '', '2026a')
         ON CONFLICT(scope_key, event, start_utc, recurrence_id)
         DO UPDATE SET end_utc = excluded.end_utc",
        (start.as_str(), end.as_str()),
    )
    .unwrap();
    assert_eq!(table_count(&conn, "event_occurrence"), 1);
}

/// A stored value v18 cannot read as an instant is left exactly as it was, and so is a NULL: the
/// step rewrites what it understands and fails on nothing else.
#[test]
fn v18_leaves_a_value_it_cannot_read_as_it_was() {
    let mut conn = Connection::open_in_memory().unwrap();
    run(&mut conn, &MIGRATIONS[..17]).unwrap();
    conn.execute_batch(
        "INSERT INTO pending_op
             (account, idempotency_key, resource_key, depends_on, payload, state, token,
              settled_at)
         VALUES ('a', 'k1', 'r', '[]', '{}', 'Succeeded', 0, 'not an instant'),
                ('a', 'k2', 'r', '[]', '{}', 'Pending', 0, NULL),
                ('a', 'k3', 'r', '[]', '{}', 'Succeeded', 0, '2026-10-06T10:00:00Z');",
    )
    .unwrap();

    migrate(&mut conn).unwrap();

    let settled: Vec<Option<String>> = conn
        .prepare("SELECT settled_at FROM pending_op ORDER BY idempotency_key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        settled,
        [
            Some("not an instant".to_owned()),
            None,
            Some("2026-10-06T10:00:00.000000000Z".to_owned()),
        ]
    );
}
