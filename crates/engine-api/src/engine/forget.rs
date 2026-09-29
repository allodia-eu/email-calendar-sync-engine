//! Forgetting what the store holds for an account: all of it when the account is removed, or
//! one domain of it when that domain stops being synced.

use engine_core::{ids::AccountId, sync::SearchDomain};
use engine_sync::rebuild_people_index;

use super::map_sync_error;
use crate::{ApiError, Engine};

impl Engine {
    /// Purges every durable trace of `account` from the local store — its synced
    /// objects, the derived search/occurrence rows, its sync scopes and cursors, the
    /// queued outbox ops, and the cached message bodies. The host calls this when it
    /// **removes** an account, so that a later re-add of the same login starts clean:
    /// account ids derive from the address, so a re-add hits the same scopes, and
    /// without this it would resume from stale cursors over orphaned rows (and, on a
    /// server without QRESYNC, never expunge mail deleted while the account was gone).
    ///
    /// The destructive counterpart of [`reset`](Self::reset): reset only clears cursors
    /// so the next sync reconciles the still-present objects; this drops the objects and
    /// forgets the scopes outright. Run it after the account is detached from the
    /// runtime, with no sync of it in flight. The content-addressed blobs on disk are
    /// deduplicated and carry no refcount, so no row owns one; follow this with
    /// [`sweep_unreferenced_blobs`](Self::sweep_unreferenced_blobs) to reclaim the ones
    /// this account was the last to name.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn forget_account(&self, account: &AccountId) -> Result<(), ApiError> {
        self.store.forget_account(account).await?;
        Ok(())
    }

    /// Purges one `domain` of `account` (its mail, its calendars or its contacts) and leaves
    /// the other two as they are: their objects, scopes and cursors, so their next sync is
    /// the delta it would have been. The host calls this when it stops syncing that domain
    /// for the account, so that syncing it again later starts from a full snapshot rather
    /// than a cursor over rows that are gone.
    ///
    /// What goes with the domain: its sync scopes with their cursors, objects and derived
    /// rows, its queued writes, and the caches only it fills (message bodies and raw sources
    /// and the sent-mail recipient history for mail; contact photos for contacts). A
    /// recipient the user asked to forget stays forgotten. Forgetting contacts rebuilds the
    /// unified people index from the sources that remain, so nobody is left listed from a
    /// card this account no longer holds.
    ///
    /// Stop the domain's syncs and writes first: like [`forget_account`](Self::forget_account)
    /// this takes no lease. A scope or queued write whose domain this build cannot tell is
    /// kept; forgetting the account removes it. Follow with
    /// [`sweep_unreferenced_blobs`](Self::sweep_unreferenced_blobs) to reclaim the files.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure, or [`ApiError::Sync`] if the people
    /// index cannot be rebuilt after contacts were forgotten; the purge itself has then
    /// already been committed.
    pub async fn forget_account_domain(
        &self,
        account: &AccountId,
        domain: SearchDomain,
    ) -> Result<(), ApiError> {
        self.store.forget_account_domain(account, domain).await?;
        if domain == SearchDomain::Contacts {
            rebuild_people_index(&self.store)
                .await
                .map_err(map_sync_error)?;
        }
        Ok(())
    }
}
