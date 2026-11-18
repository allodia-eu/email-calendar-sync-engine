//! The per-scope **expansion window**: the horizon an event scope's occurrence rows are
//! materialized over, and the zone they were resolved through.
//!
//! Kept beside the scope's lease and cursor because it is the same kind of fact — what this
//! scope's rows currently *mean* — and written under the same fencing token as the rows
//! themselves. `store-and-sync.md` states why it must be the store's: a pass that re-derives
//! one changed event clears that event's occurrence rows outright (or a moved event would
//! ghost), so it has to put back the whole window the host expanded, not whichever horizon
//! its caller happened to hold.
//!
//! The scope's **collection tag** lives here too, for the same reason: it says which state of
//! the collection the scope's rows are, and it is written with the cursor. So does its **fetch
//! window**, the date range a windowed event scope's cursor was opened over: inside it the rows
//! are the server's whole calendar, outside it only what happened to arrive.

use engine_core::{
    time::{ExpansionWindow, Horizon, TimeZoneId},
    version::CollectionTag,
};
use engine_store::Result;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{convert, scope_ops, sql};

/// What a cursor apply records with the cursor.
pub(crate) struct CursorRecord {
    /// The cursor itself.
    pub(crate) cursor: String,
    /// The collection tag; `None` clears the recorded one.
    pub(crate) collection_tag: Option<String>,
    /// The window the cursor was opened over; `None` leaves the recorded one.
    pub(crate) fetch_window: Option<Horizon>,
}

#[cfg(test)]
impl CursorRecord {
    /// A cursor recorded alone: no tag, and the fetch window left as it is.
    pub(crate) fn bare(cursor: &str) -> Self {
        Self {
            cursor: cursor.to_owned(),
            collection_tag: None,
            fetch_window: None,
        }
    }
}

/// Records `record` on the scope, inside the apply's transaction.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure.
pub(crate) fn record_cursor(
    tx: &Transaction<'_>,
    scope_key: &str,
    record: &CursorRecord,
) -> Result<()> {
    sql::execute(
        tx,
        "UPDATE sync_scope SET cursor = ?1, collection_tag = ?2 WHERE scope_key = ?3",
        (&record.cursor, record.collection_tag.as_deref(), scope_key),
    )?;
    if let Some(window) = record.fetch_window {
        sql::execute(
            tx,
            "UPDATE sync_scope SET fetch_start = ?1, fetch_end = ?2 WHERE scope_key = ?3",
            (
                convert::instant_to_text(window.start()),
                convert::instant_to_text(window.end()),
                scope_key,
            ),
        )?;
    }
    Ok(())
}

/// Records the window this scope's occurrence rows are now materialized over, gated by
/// the lease token — the same gate as the write that produced them.
///
/// # Errors
///
/// Returns [`StoreError::StaleLease`] if the token is no longer current, or
/// [`StoreError::Backend`] on a backend failure.
pub(crate) fn set_expansion_window(
    conn: &mut Connection,
    scope_key: &str,
    token: u64,
    window: &ExpansionWindow,
) -> Result<()> {
    let tx = conn.transaction().map_err(convert::backend)?;
    scope_ops::check_token(&tx, scope_key, token)?;
    tx.execute(
        "UPDATE sync_scope
            SET horizon_start = ?1, horizon_end = ?2, expansion_zone = ?3
          WHERE scope_key = ?4",
        (
            convert::instant_to_text(window.horizon.start()),
            convert::instant_to_text(window.horizon.end()),
            window.zone.as_str(),
            scope_key,
        ),
    )
    .map_err(convert::backend)?;
    tx.commit().map_err(convert::backend)?;
    Ok(())
}

/// The window a scope's occurrence rows are materialized over, or `None` if nothing has
/// expanded them yet (or the scope holds no occurrences at all).
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure, or if a stored window cannot be
/// parsed back (which would be store corruption, not host input).
pub(crate) fn expansion_window(
    conn: &Connection,
    scope_key: &str,
) -> Result<Option<ExpansionWindow>> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT horizon_start, horizon_end, expansion_zone FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(convert::backend)?;
    let Some((Some(start), Some(end), Some(zone))) = row else {
        return Ok(None);
    };
    let horizon = Horizon::new(
        convert::parse_instant(&start)?,
        convert::parse_instant(&end)?,
    )
    .map_err(convert::backend)?;
    let zone = TimeZoneId::iana(&zone).map_err(convert::backend)?;
    Ok(Some(ExpansionWindow::new(horizon, zone)))
}

/// The collection tag recorded with a scope's cursor, if one was.
///
/// # Errors
///
/// Returns [`StoreError::Backend`] on a backend failure.
pub(crate) fn collection_tag(conn: &Connection, scope_key: &str) -> Result<Option<CollectionTag>> {
    let tag: Option<Option<String>> = conn
        .query_row(
            "SELECT collection_tag FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(convert::backend)?;
    Ok(tag.flatten().map(CollectionTag::new))
}

/// The window a windowed event scope's cursor was opened over, if one was recorded.
///
/// # Errors
///
/// Returns [`StoreError::Backend`](engine_store::StoreError::Backend) on a backend failure, or if
/// a stored window cannot be parsed back (store corruption).
pub(crate) fn fetch_window(conn: &Connection, scope_key: &str) -> Result<Option<Horizon>> {
    let row: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT fetch_start, fetch_end FROM sync_scope WHERE scope_key = ?1",
            [scope_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(convert::backend)?;
    let Some((Some(start), Some(end))) = row else {
        return Ok(None);
    };
    Horizon::new(
        convert::parse_instant(&start)?,
        convert::parse_instant(&end)?,
    )
    .map(Some)
    .map_err(convert::backend)
}
