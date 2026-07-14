//! The per-scope **expansion window**: the horizon an event scope's occurrence rows are
//! materialized over, and the zone they were resolved through.
//!
//! Kept beside the scope's lease and cursor because it is the same kind of fact — what this
//! scope's rows currently *mean* — and written under the same fencing token as the rows
//! themselves. `store-and-sync.md` states why it must be the store's: a pass that re-derives
//! one changed event clears that event's occurrence rows outright (or a moved event would
//! ghost), so it has to put back the whole window the host expanded, not whichever horizon
//! its caller happened to hold.

use engine_core::time::{ExpansionWindow, Horizon, TimeZoneId};
use engine_store::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::{convert, scope_ops};

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
