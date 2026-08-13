//! Filling a new table from what the store already holds, at migration time.
//!
//! The store is mechanical everywhere else — it writes derived rows, it never computes them
//! (`store-and-sync.md`). A backfill is the one place that cannot hold: the rows it must produce
//! are a *function* of the normalized objects, and writing that function a second time in SQL over
//! the stored JSON would leave two definitions of one projection to drift apart. So it
//! deserializes the object and runs the engine's own projection, exactly as the sync path does.
//!
//! The alternative is worse than the exception: without it a reshaping step clears the cursors and
//! the user watches their whole mailbox download again.

use engine_core::{mail::Message, search_index::project_message};
use engine_store::Result;
use rusqlite::Transaction;

use crate::{derived_ops, sql};

/// How many payloads a backfill holds at once.
///
/// A mailbox is unbounded and a payload is about a kilobyte, so reading the whole join into a
/// `Vec` would make the migration's memory a function of how much mail the user has. The scan is
/// keyset-paged over `object`'s own primary key, which is an index range scan per page.
const PAGE: usize = 1_000;

/// Fills `message` from the normalized mail already in `object` (schema v9).
///
/// `mail_index` is the liveness and mail-ness filter: its rows are cleared with their object, and
/// only mail objects ever had one, so joining it selects exactly the messages that belong here and
/// nothing from a calendar or contact scope. v10 drops it afterwards.
///
/// A payload that will not deserialize is skipped rather than failing the migration: the read path
/// this replaces could not decode that message either, so skipping changes nothing a user sees,
/// while failing would leave the store unopenable.
pub(crate) fn messages_from_objects(tx: &Transaction<'_>) -> Result<()> {
    let mut after = (String::new(), String::new());
    loop {
        let page: Vec<(String, String, String, String)> = sql::query_all(
            tx,
            "SELECT o.scope_key, o.provider_key, s.account, o.payload
               FROM object o
               JOIN mail_index mi
                 ON mi.scope_key = o.scope_key AND mi.provider_key = o.provider_key
               JOIN sync_scope s ON s.scope_key = o.scope_key
              WHERE (o.scope_key, o.provider_key) > (?1, ?2)
              ORDER BY o.scope_key, o.provider_key
              LIMIT ?3",
            (&after.0, &after.1, i64::try_from(PAGE).unwrap_or(i64::MAX)),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let Some((last_scope, last_key, ..)) = page.last() else {
            return Ok(());
        };
        after = (last_scope.clone(), last_key.clone());
        for (scope_key, _, account, payload) in page {
            let Ok(message) = serde_json::from_str::<Message>(&payload) else {
                continue;
            };
            derived_ops::upsert_message(tx, &scope_key, &account, &project_message(&message).row)?;
        }
    }
}
