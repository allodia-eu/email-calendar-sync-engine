//! How much mail the store keeps for an account: clearing its mail cursors, pruning it to a
//! narrower window offline, and dropping and sweeping cached bytes.

use engine_core::{
    ids::AccountId,
    sync::{MailboxWindows, SearchDomain},
};
use engine_store::{MessageSourceCache, PruneReport, SourcesDropped, SweepReport};

use crate::{ApiError, Engine};

impl Engine {
    /// Clears just the **mail** scopes' sync cursors, so the next [`Engine::sync_mail`]
    /// re-snapshots them. The targeted counterpart of [`Engine::reset`]: it reconciles
    /// mail with the server without clearing the calendar or re-fetching the whole
    /// account. A plain `sync_mail` delta already reconciles flag, move and expunge changes
    /// on every IMAP server: incrementally with QRESYNC, and without it by reading every
    /// held message's flags and the set still present (`imap-smtp.md`). This is for a host
    /// that wants to force a full mail re-snapshot, or to enforce a narrower window on a
    /// reachable account; a plain `sync_mail` after it reconciles, since the cleared scopes
    /// snapshot. A **wider** window needs no clear: the next `sync_mail` under it fetches only
    /// what it adds, and continues a widening a killed pass left (`store-and-sync.md`). A mail
    /// sync running meanwhile is stopped at its next commit rather than writing its
    /// cursor back, and the scope can be claimed at once.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn clear_mail_cursors(&self, account: &AccountId) -> Result<(), ApiError> {
        for scope in self.scopes_in(account, SearchDomain::Mail).await? {
            self.store.clear_scope_cursor(&scope).await?;
        }
        Ok(())
    }

    /// Prunes `account`'s locally-stored mail dated **before** `window`'s floor, so a
    /// reduced sync depth holds even **offline** — with no provider round trip. When the
    /// account is reachable, a host narrows depth by clearing the mail cursors and
    /// re-syncing: the provider snapshot under the narrower `window` tombstones the
    /// out-of-window rows. This is the counterpart for a disconnected account: it drops
    /// the same mail locally, producing the state that re-snapshot would, so the app can
    /// enforce the new depth immediately and wait to reconcile until the next sync.
    ///
    /// `window` is the account's [`SyncWindow`](crate::SyncWindow), or the
    /// [`MailboxWindows`] the account syncs under when a mailbox is held further back; a
    /// message filed in a deepened mailbox is kept while that mailbox's window admits it, so
    /// this never removes what a deeper pass fetched.
    ///
    /// It keeps in-window and undated mail (an undated message is not provably out of
    /// window), non-mail data, account metadata, and every other account; each removed
    /// message takes its derived search/thread/occurrence rows with it (the same
    /// tombstone a sync applies) **and its cached body and raw source**, which are the bulk
    /// of what the mail occupied. An unbounded `window` is a no-op. It advances no cursor,
    /// so a later network sync resumes normally; follow it with
    /// [`sweep_unreferenced_blobs`](Self::sweep_unreferenced_blobs) and
    /// [`vacuum`](Self::vacuum) to reclaim the freed files and pages. Returns a
    /// [`PruneReport`](engine_store::PruneReport) with the count removed.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn prune_account_mail_outside_window(
        &self,
        account: &AccountId,
        window: impl Into<MailboxWindows>,
    ) -> Result<PruneReport, ApiError> {
        Ok(self
            .store
            .prune_account_mail_outside_window(account, window)
            .await?)
    }

    /// Forgets `account`'s cached raw sources over `octets`, for a host lowering a message-size
    /// cap — the counterpart of the cap the same host applies when deciding what to pre-fetch.
    ///
    /// It removes **bytes, not mail**: the message rows and their extracted body text stay, so
    /// the list, the threads and body search are all unchanged and only the offline copy of the
    /// heaviest messages goes. Opening one re-fetches and re-caches it, exactly as it would have
    /// before it was ever warmed. Follow with
    /// [`sweep_unreferenced_blobs`](Self::sweep_unreferenced_blobs) and [`vacuum`](Self::vacuum)
    /// to turn it into free disk — this drops the rows that *name* the files, and the sweep is
    /// the only place that knows whether another row still names the same content.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn drop_message_sources_over(
        &self,
        account: &AccountId,
        octets: u64,
    ) -> Result<SourcesDropped, ApiError> {
        Ok(self
            .store
            .drop_message_sources_over(account, octets)
            .await?)
    }

    /// Deletes every content-addressed blob no store row names any more — the raw
    /// message sources (and contact photos) left behind when the mail that cached them
    /// was removed.
    ///
    /// Blobs are named by the hash of their bytes so two copies of one message share one
    /// file, which means no row owns a file and the delete that drops the last row naming
    /// a hash cannot free it. A host therefore runs this after anything that drops mail in
    /// quantity — [`prune_account_mail_outside_window`](Self::prune_account_mail_outside_window),
    /// a narrower-window re-snapshot, or [`forget_account`](Self::forget_account) — and
    /// pairs it with [`vacuum`](Self::vacuum), which reclaims the database half of the same
    /// space. Raw sources run 1–15 MB apiece, so this is the larger half.
    ///
    /// Safe to run at any time: it takes no lease, and a blob removed a moment early reads
    /// back as a cache miss the caller re-fetches, never as wrong bytes. Returns a
    /// [`SweepReport`](engine_store::SweepReport) with the count and bytes freed.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a filesystem or backend failure.
    pub async fn sweep_unreferenced_blobs(&self) -> Result<SweepReport, ApiError> {
        Ok(self.store.sweep_unreferenced_blobs().await?)
    }
}
