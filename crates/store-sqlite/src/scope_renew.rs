//! Renewing a scope lease while its pass runs.

use engine_core::time::UtcDateTime;
use engine_store::{Result, StoreError};
use rusqlite::Connection;

use crate::{convert, sql};

/// Moves the scope's lease expiry to `expiry` while `token` still holds it; a lease another
/// worker has claimed since, or one its own pass already released, is stale.
pub(crate) fn renew(
    conn: &Connection,
    scope_key: &str,
    token: u64,
    expiry: UtcDateTime,
) -> Result<()> {
    let written = sql::execute(
        conn,
        "UPDATE sync_scope SET lease_expiry = ?1
          WHERE scope_key = ?2 AND token = ?3 AND lease_expiry IS NOT NULL",
        (
            convert::instant_to_text(expiry),
            scope_key,
            convert::generation_to_i64(token)?,
        ),
    )?;
    if written == 0 {
        return Err(StoreError::StaleLease);
    }
    Ok(())
}
