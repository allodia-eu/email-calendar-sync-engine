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

use engine_core::{
    ids::{MessageIdHeader, ThreadId},
    mail::Message,
    search_index::{MailRow, project_message},
    version::{ChangeKey, ETag},
};
use engine_store::Result;
use rusqlite::Transaction;

use crate::{convert, sql};

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
            insert_v9_row(tx, &scope_key, &account, &project_message(&message).row)?;
        }
    }
}

/// Inserts one message row using **exactly the columns v9 created**.
///
/// Deliberately not the shared `derived_ops::upsert_message`: that one follows the *live* schema,
/// which has moved on (v11 added the state columns), and a backfill runs against the schema as of
/// its own step. Sharing it means a later migration silently breaks an earlier one's backfill —
/// which is how this was found. **A backfill's SQL is pinned to its own version; it does not
/// borrow the live write path.**
fn insert_v9_row(
    tx: &Transaction<'_>,
    scope_key: &str,
    account: &str,
    row: &MailRow,
) -> Result<()> {
    sql::execute(
        tx,
        "INSERT INTO message (scope_key, provider_key, account, thread_id, message_id, date_utc,
                              flags, has_attachment, from_name, from_addr, subject, preview)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(scope_key, provider_key) DO UPDATE SET
             account = excluded.account,
             thread_id = excluded.thread_id,
             message_id = excluded.message_id,
             date_utc = excluded.date_utc,
             flags = excluded.flags,
             has_attachment = excluded.has_attachment,
             from_name = excluded.from_name,
             from_addr = excluded.from_addr,
             subject = excluded.subject,
             preview = excluded.preview",
        rusqlite::params![
            scope_key,
            row.key.as_str(),
            account,
            row.thread_id.as_ref().map(ThreadId::as_str),
            row.message_id.as_ref().map(MessageIdHeader::as_str),
            row.date_utc.map(convert::instant_to_text),
            i64::from(row.flags.bits()),
            i64::from(row.has_attachment),
            row.from_name.as_deref(),
            row.from_addr.as_deref(),
            row.subject.as_deref(),
            row.preview.as_deref(),
        ],
    )?;
    Ok(())
}

/// Backfills v11's state columns from the payloads that still carry them.
///
/// `revisions` and `last_modified` were stored inside the normalized payload until the split
/// that gave a message's mutable state its own home. They are still *in* those payloads, so this
/// lifts them across rather than re-downloading anything; from here on the payload no longer
/// carries them and the columns are where they live.
///
/// A payload that will not deserialize is skipped, for the same reason as
/// [`messages_from_objects`]: a store that will not open is worse than a message whose revision
/// token is unknown until its next sync.
pub(crate) fn message_state_from_objects(tx: &Transaction<'_>) -> Result<()> {
    let mut after = (String::new(), String::new());
    loop {
        let page: Vec<(String, String, String)> = sql::query_all(
            tx,
            "SELECT o.scope_key, o.provider_key, o.payload
               FROM object o
               JOIN message m
                 ON m.scope_key = o.scope_key AND m.provider_key = o.provider_key
              WHERE (o.scope_key, o.provider_key) > (?1, ?2)
              ORDER BY o.scope_key, o.provider_key
              LIMIT ?3",
            (&after.0, &after.1, i64::try_from(PAGE).unwrap_or(i64::MAX)),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let Some((last_scope, last_key, ..)) = page.last() else {
            return Ok(());
        };
        after = (last_scope.clone(), last_key.clone());
        for (scope_key, provider_key, payload) in page {
            let Ok(message) = serde_json::from_str::<Message>(&payload) else {
                continue;
            };
            // Pinned to v11's columns for the same reason as `insert_v9_row` above.
            sql::execute(
                tx,
                "UPDATE message SET last_modified = ?3, etag = ?4, change_key = ?5, mod_seq = ?6
                 WHERE scope_key = ?1 AND provider_key = ?2",
                rusqlite::params![
                    scope_key,
                    provider_key,
                    message.last_modified.map(convert::instant_to_text),
                    message.revisions.etag.as_ref().map(ETag::as_str),
                    message.revisions.change_key.as_ref().map(ChangeKey::as_str),
                    message
                        .revisions
                        .mod_seq
                        .as_ref()
                        .and_then(|m| i64::try_from(m.get()).ok()),
                ],
            )?;
        }
    }
}
