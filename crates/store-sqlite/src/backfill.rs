//! Data moves that must commit with the DDL step that needs them.
//!
//! A migration that adds a shape whose contents are a function of what the store already holds
//! cannot be DDL alone: leaving the new table empty and waiting for the next sync to fill it means
//! a database that is at the new version and *wrong* until then. These functions run inside the
//! same transaction as their step ([`crate::migrations`]), so a database is never at version `n`
//! with version `n`'s table unpopulated.
//!
//! They fill the new shape from `object`, which already holds the normalized record, by running
//! the engine's own projection over it — never a second parser written here. A backfill is what
//! keeps a reshaping migration from costing the user a re-**sync**: the bytes are already local.

use engine_core::{ids::ProviderKey, mail::StoredContent, search_index::project_refs};
use engine_store::Result;
use rusqlite::Transaction;

use crate::{convert::backend, sql};

/// Fills `msgid_ref` from the stored mail payloads (schema v10).
///
/// Joins `object` to `message` because that join *is* "the mail objects, with the account each is
/// filed under" — a payload with no message row is not mail, and no second predicate is needed to
/// say so.
///
/// A payload that will not decode is skipped rather than fatal. It is already unreadable by every
/// other path, so failing the migration over it would leave the user with a store that cannot
/// open at all; the message simply threads alone until a re-sync replaces it.
pub(crate) fn msgid_refs(tx: &Transaction<'_>) -> Result<()> {
    // Streamed, not collected. The payloads are the bulk of the database — a mailbox this walks
    // is hundreds of megabytes of them — and reading them into a `Vec` first would hold the whole
    // lot in memory at once, on a phone, during a launch. One row at a time costs nothing extra:
    // the graph rows are written as each payload is decoded and the payload is then dropped.
    let mut mail = tx
        .prepare(
            "SELECT o.scope_key, o.provider_key, m.account, o.payload
               FROM object o
               JOIN message m ON m.scope_key = o.scope_key AND m.provider_key = o.provider_key",
        )
        .map_err(backend)?;
    let mut rows = mail.query([]).map_err(backend)?;
    while let Some(row) = rows.next().map_err(backend)? {
        let scope_key: &str = row.get_ref(0).map_err(backend)?.as_str().map_err(backend)?;
        let provider_key: &str = row.get_ref(1).map_err(backend)?.as_str().map_err(backend)?;
        let account: &str = row.get_ref(2).map_err(backend)?.as_str().map_err(backend)?;
        let payload: &str = row.get_ref(3).map_err(backend)?.as_str().map_err(backend)?;

        let Ok(content) = serde_json::from_str::<StoredContent>(payload) else {
            continue;
        };
        let Ok(key) = ProviderKey::new(provider_key) else {
            continue;
        };
        for graph_row in project_refs(&key, &content.envelope, content.thread.as_ref()) {
            // Through the statement cache, like every other write here. Measured, it makes no
            // difference to this pass — decoding the payloads dominates it — but a write that
            // compiles its SQL a few hundred thousand times is not a thing to leave in place on
            // the grounds that something slower is next to it.
            sql::execute(
                tx,
                "INSERT INTO msgid_ref (scope_key, provider_key, account, msgid, owned)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(scope_key, provider_key, msgid)
                 DO UPDATE SET owned = MAX(owned, excluded.owned)",
                rusqlite::params![
                    scope_key,
                    provider_key,
                    account,
                    graph_row.msgid.as_str(),
                    i64::from(graph_row.owned),
                ],
            )?;
        }
    }
    Ok(())
}

/// Every column that holds an instant, as `(table, column)`. Each is written through
/// [`convert::instant_to_text`](crate::convert::instant_to_text) and nowhere else, and the guard
/// test in `migrations` fails when a column holding an instant is missing here.
pub(crate) const INSTANT_COLUMNS: &[(&str, &str)] = &[
    ("sync_scope", "lease_expiry"),
    ("sync_scope", "horizon_start"),
    ("sync_scope", "horizon_end"),
    ("event_occurrence", "start_utc"),
    ("event_occurrence", "end_utc"),
    ("event_occurrence", "recurrence_id"),
    ("pending_op", "lease_expiry"),
    ("pending_op", "next_attempt_at"),
    ("pending_op", "settled_at"),
    ("message", "date_utc"),
    ("message", "last_modified"),
    ("message_source", "fetched_at"),
    ("message_body", "fetched_at"),
    ("contact_photo", "fetched_at"),
    ("recipient_observation", "sent_at"),
];

/// Rewrites every stored instant in its fixed-width form (schema v18).
///
/// The text an instant was stored as dropped a zero fraction and trailing zeros, which does not
/// sort as text within one second; SQL orders and filters these columns as text. Rewritten per
/// distinct value, so a table keyed on one of them (`event_occurrence` is unique on `start_utc`)
/// never holds two spellings of one instant, which a re-derived row in the new form would
/// otherwise add beside the old. A value that does not parse is left as it is: it was unreadable
/// before, and failing the migration over it would leave a store that cannot open.
pub(crate) fn fixed_width_instants(tx: &Transaction<'_>) -> Result<()> {
    for (table, column) in INSTANT_COLUMNS {
        let values: Vec<String> = {
            let mut stmt = tx
                .prepare(&format!(
                    "SELECT DISTINCT {column} FROM {table} WHERE {column} IS NOT NULL AND {column} != ''"
                ))
                .map_err(backend)?;
            let rows = stmt.query_map([], |row| row.get(0)).map_err(backend)?;
            rows.collect::<rusqlite::Result<_>>().map_err(backend)?
        };
        for value in values {
            let Ok(instant) = crate::convert::parse_instant(&value) else {
                continue;
            };
            let fixed = crate::convert::instant_to_text(instant);
            if fixed != value {
                sql::execute(
                    tx,
                    &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                    (value.as_str(), fixed.as_str()),
                )?;
            }
        }
    }
    Ok(())
}
