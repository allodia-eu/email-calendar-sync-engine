//! Local depth narrowing: drop an account's mail that falls outside a narrowed
//! [`SyncWindow`], without a live provider.
//!
//! The reachable way to narrow sync depth is to clear the mail cursors and re-sync:
//! the provider snapshot under the narrower window tombstones the out-of-window rows
//! (`store-and-sync.md`). That path needs the provider. When the account cannot reach
//! it, a host still wants the reduced depth to hold locally — so this prune reproduces
//! the *same* end state offline, tombstoning exactly the mail a narrower-window
//! snapshot would have.
//!
//! It filters on `message.date_utc`, which is the message's `received_at` (falling
//! back to `sent_at`) — the very field a provider window maps to (IMAP `SINCE`, JMAP
//! `after` on `receivedAt`, Graph `receivedDateTime ge`). Comparison is on the UTC
//! **date** prefix against the window's inclusive floor date, so a message *on* the
//! floor date is kept and only strictly-earlier mail is dropped. Mail with **no** date
//! is kept: an undated message is not provably out of window, and a prune must not
//! over-delete.
//!
//! Like [`forget_account`](crate::SqliteStore::forget_account) it is not lease-gated —
//! the store's single connection serializes it atomically against any in-flight sync,
//! and it advances no cursor, so a later delta sync resumes unaffected. It reuses the
//! scope tombstone ([`scope_ops::tombstone`]), so the derived search/thread/occurrence
//! rows and the two lease-free content caches go with each object — the caches being the
//! bulk of what the mail occupied. The blob each source row named is content-addressed and
//! shared, so the files are reclaimed by [`SqliteStore::sweep_unreferenced_blobs`] and the
//! freed database pages by [`SqliteStore::vacuum`], both of which a host runs after this.

use engine_core::{
    ids::AccountId,
    sync::{SearchDomain, SyncWindow},
};
use engine_store::{Clock, PruneReport, Result};
use rusqlite::{Connection, Transaction};

use crate::{SqliteStore, convert, scope_ops};

impl<C: Clock> SqliteStore<C> {
    /// Removes `account`'s locally-stored mail whose date falls **before** `window`'s
    /// floor, keeping in-window mail, undated mail, non-mail objects, account metadata,
    /// and every other account. This is the offline counterpart of a sync-depth
    /// reduction: after the user narrows the window a host calls this so the new depth
    /// holds even without a provider round trip, producing the same local state a
    /// narrower-window re-snapshot would (`store-and-sync.md`).
    ///
    /// An unbounded window (`SyncWindow::full`) has no floor, so nothing is outside it
    /// and this is a no-op. Each removed message takes its derived search/thread/
    /// occurrence rows with it (the same tombstone a snapshot reconciliation applies);
    /// the lease-free cached body and raw-source blob are left to eviction/`vacuum`. No
    /// sync cursor is touched, so the next network sync resumes normally.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn prune_account_mail_outside_window(
        &self,
        account: &AccountId,
        window: SyncWindow,
    ) -> Result<PruneReport> {
        let Some(floor) = window.floor() else {
            return Ok(PruneReport::default());
        };
        // The stored `date_utc` is `YYYY-MM-DDT…Z`; compare its 10-char UTC date prefix
        // against the floor's canonical `YYYY-MM-DD`, so the check is date-granular and
        // immune to any sub-second component the timestamp might carry.
        let floor = floor.to_string();
        let account = account.clone();
        self.call(move |conn| prune_account_mail(conn, &account, &floor))
            .await
    }
}

/// Tombstones every out-of-window mail object across the account's mail scopes in one
/// transaction, returning the count. Non-mail scopes are skipped, so calendar/contact
/// objects and account metadata are untouched.
fn prune_account_mail(
    conn: &mut Connection,
    account: &AccountId,
    floor: &str,
) -> Result<PruneReport> {
    let tx = conn.transaction().map_err(convert::backend)?;
    let mut messages_removed = 0;
    for scope in scope_ops::account_scopes(&tx, account)? {
        if scope.search_domain() != Some(SearchDomain::Mail) {
            continue;
        }
        let scope_key = convert::scope_key(&scope);
        for key in out_of_window_keys(&tx, &scope_key, floor)? {
            if scope_ops::tombstone(&tx, &scope_key, &key)? {
                messages_removed += 1;
            }
        }
    }
    tx.commit().map_err(convert::backend)?;
    Ok(PruneReport { messages_removed })
}

/// The provider keys of a scope's mail dated strictly before `floor` (a `YYYY-MM-DD`
/// UTC date). Rows with a `NULL` `date_utc` are excluded — an undated message is not
/// provably out of window, so it is kept. Collected up front so the read statement is
/// released before the tombstone deletes run against the same transaction.
fn out_of_window_keys(tx: &Transaction<'_>, scope_key: &str, floor: &str) -> Result<Vec<String>> {
    let mut stmt = tx
        .prepare(
            "SELECT provider_key FROM message
             WHERE scope_key = ?1 AND date_utc IS NOT NULL AND substr(date_utc, 1, 10) < ?2",
        )
        .map_err(convert::backend)?;
    let rows = stmt
        .query_map((scope_key, floor), |r| r.get::<_, String>(0))
        .map_err(convert::backend)?;
    rows.collect::<rusqlite::Result<Vec<String>>>()
        .map_err(convert::backend)
}

#[cfg(test)]
mod tests;
