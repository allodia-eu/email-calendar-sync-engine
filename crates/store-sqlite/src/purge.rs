//! Account teardown: drop every trace of one account from the store.
//!
//! The targeted, destructive counterpart of the cursor-only `reset_sync`
//! (`store-and-sync.md`): a host removing an account calls this so a later re-add of
//! the same login starts clean rather than inheriting stale cursors and orphan rows.
//! It is not lease-gated — the account is already detached from the runtime, so no
//! sync should be in flight — and it commits every table in one transaction.

use engine_core::ids::AccountId;
use engine_store::{Clock, Result};
use rusqlite::Connection;

use crate::{SqliteStore, convert::backend};

/// Tables keyed by `scope_key`: every row whose scope belongs to the account is
/// removed via the `sync_scope` sub-select. `fts_doc`'s FTS5 shadow (`fts_index`)
/// follows through its delete trigger, so it is not listed here.
const SCOPE_TABLES: &[&str] = &[
    "object",
    "fts_doc",
    "event_occurrence",
    "mail_index",
    "mail_address",
    "membership",
    "event_index",
    "event_participant",
    "embedding",
];

/// Tables keyed directly by `account`. `message_body`'s FTS5 shadow
/// (`message_body_fts`) follows through its delete trigger. `sync_scope` is also
/// account-keyed but is deleted last (below), since the [`SCOPE_TABLES`] deletes
/// resolve their sub-select against it.
const ACCOUNT_TABLES: &[&str] = &["pending_op", "message_source", "message_body"];

impl<C: Clock> SqliteStore<C> {
    /// Purges every durable trace of `account`: its synced objects, the derived
    /// search/occurrence rows, its sync scopes and cursors, the queued outbox ops, and
    /// the cached message bodies — all in one transaction. A host calls this when it
    /// **removes** an account, so a later re-add of the same login (account ids derive
    /// from the address, so it hits the same scopes) starts from a clean slate instead
    /// of resuming from stale cursors over orphaned rows.
    ///
    /// This is the destructive counterpart of [`reset_sync`](Self::reset_sync): reset
    /// clears only the cursors and lets the next sync reconcile the still-present
    /// objects, whereas this drops the objects too and forgets the scopes entirely.
    ///
    /// The content-addressed raw-message blobs on disk are **not** deleted here: they
    /// are deduplicated by content hash (a blob may back another account) and carry no
    /// refcount, so they are left to the separate size-based eviction path
    /// (`message_source.fetched_at`). A re-add re-adopts an identical blob on the next
    /// on-demand fetch rather than re-downloading it.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn forget_account(&self, account: &AccountId) -> Result<()> {
        let account = account.as_str().to_owned();
        self.call(move |conn| purge_account(conn, &account)).await
    }
}

/// Deletes every row belonging to `account` across the scope-keyed and account-keyed
/// tables in one transaction, `sync_scope` last so the scope-keyed sub-selects still
/// resolve against it.
fn purge_account(conn: &mut Connection, account: &str) -> Result<()> {
    let tx = conn.transaction().map_err(backend)?;
    for table in SCOPE_TABLES {
        tx.execute(
            &format!(
                "DELETE FROM {table} \
                 WHERE scope_key IN (SELECT scope_key FROM sync_scope WHERE account = ?1)"
            ),
            [account],
        )
        .map_err(backend)?;
    }
    for table in ACCOUNT_TABLES {
        tx.execute(
            &format!("DELETE FROM {table} WHERE account = ?1"),
            [account],
        )
        .map_err(backend)?;
    }
    tx.execute("DELETE FROM sync_scope WHERE account = ?1", [account])
        .map_err(backend)?;
    tx.commit().map_err(backend)?;
    Ok(())
}

#[cfg(test)]
mod tests;
