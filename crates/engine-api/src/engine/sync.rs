//! Provider-driven sync, cache-reset/vacuum maintenance, and the streaming and
//! per-folder sync methods on `Engine`.

use engine_core::{
    ids::AccountId,
    sync::{SearchDomain, SyncWindow},
    time::TimeZoneId,
};
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{PruneReport, SyncApplied};
use engine_sync::{
    CalendarSyncReport, MailSyncReport, StreamTuning, SyncObserver, ThreadDeriveReport,
    derive_mail_threads, sync_calendar, sync_email_streamed, sync_mail, sync_mail_streamed,
    sync_mailbox_list,
};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Syncs one account's mail from `provider`: mailbox containers first, then
    /// email members, each through the claim → fetch → derive → apply → release
    /// cycle with `StaleLease` recovery (`store-and-sync.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this account's mail
    /// scope, or [`ApiError::Sync`] if the provider fetch fails or the store rejects
    /// the apply.
    pub async fn sync_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<MailSyncReport, ApiError> {
        sync_mail(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Derives and persists thread ids for the account's mail that has none — IMAP and
    /// other providers without native threading — grouping messages across folders by
    /// their `Message-ID`/`In-Reply-To`/`References` headers (so a sent reply and its
    /// received original share a thread). A no-op for providers that assign thread ids
    /// themselves. Run after [`Engine::sync_mail`]; subsequent [`Engine::messages`]
    /// reads then carry the grouped `thread_id`.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if a sync already holds a mail scope, or
    /// [`ApiError::Sync`] if the store rejects the apply.
    pub async fn derive_mail_threads(
        &self,
        account: &AccountId,
    ) -> Result<ThreadDeriveReport, ApiError> {
        derive_mail_threads(&self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Resets the local cache: clears every sync cursor so the next sync re-fetches and
    /// re-normalizes the account from scratch — the host's "reset / full refetch". The
    /// durable outbox (queued sends) is preserved. Sync afterwards to repopulate; until
    /// then the previously-synced objects remain readable and are reconciled by that
    /// re-snapshot. The same clear happens automatically when the engine's
    /// `NORMALIZER_VERSION` changes (`store-and-sync.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn reset(&self) -> Result<(), ApiError> {
        self.store.reset_sync().await?;
        Ok(())
    }

    /// Compacts the on-disk database, reclaiming the free pages left after objects are
    /// deleted — e.g. the out-of-window mail a re-snapshot tombstones once a
    /// [`reset`](Self::reset) (or a sync-depth reduction) and its follow-up sync have
    /// dropped everything past the window. SQLite holds a file at its high-water mark and
    /// reuses freed pages, so the on-disk size never falls on its own; a host calls this
    /// after a reset's re-sync settles to shrink the file back to the live data's size. It
    /// rewrites the whole database, so it needs transient free disk space about the size of
    /// the database and briefly serializes the store — not for a hot path.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn vacuum(&self) -> Result<(), ApiError> {
        self.store.vacuum().await?;
        Ok(())
    }

    /// Clears just the **mail** scopes' sync cursors, so the next [`Engine::sync_mail`]
    /// re-snapshots them. The targeted counterpart of [`Engine::reset`]: it reconciles
    /// mail with the server without clearing the calendar or re-fetching the whole
    /// account. Against a **QRESYNC** IMAP server a plain `sync_mail` delta already
    /// reconciles flag, move, and expunge changes incrementally (`imap-smtp.md`), so
    /// this is the **fallback** for a server without QRESYNC (where a delta brings new
    /// arrivals only) or a host that wants to force a full mail re-snapshot; a plain
    /// `sync_mail` after it reconciles, since the cleared scopes snapshot.
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
    /// runtime, with no sync of it in flight. The content-addressed raw-message blobs on
    /// disk are left to size-based eviction (they are deduplicated and carry no
    /// refcount).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn forget_account(&self, account: &AccountId) -> Result<(), ApiError> {
        self.store.forget_account(account).await?;
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
    /// It keeps in-window and undated mail (an undated message is not provably out of
    /// window), non-mail data, account metadata, and every other account; each removed
    /// message takes its derived search/thread/occurrence rows with it (the same
    /// tombstone a sync applies). An unbounded `window` is a no-op. It advances no
    /// cursor, so a later network sync resumes normally; run [`vacuum`](Self::vacuum)
    /// afterwards to reclaim the freed pages. Returns a
    /// [`PruneReport`](engine_store::PruneReport) with the count removed.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn prune_account_mail_outside_window(
        &self,
        account: &AccountId,
        window: SyncWindow,
    ) -> Result<PruneReport, ApiError> {
        Ok(self
            .store
            .prune_account_mail_outside_window(account, window)
            .await?)
    }

    /// Syncs one account's calendars from `provider`: calendar containers first,
    /// then events, expanding each event's occurrences over `horizon` (resolving
    /// floating times through `host_zone`) before the commit
    /// (`calendar-semantics.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this account's
    /// calendar scope, or [`ApiError::Sync`] if the provider fetch fails or the
    /// store rejects the apply.
    pub async fn sync_calendar<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<CalendarSyncReport, ApiError> {
        sync_calendar(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            horizon,
            host_zone,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Syncs **only** one account's mailbox list (folder discovery) from `provider`,
    /// skipping the email members. The once-per-account container step a host runs
    /// before fanning out the per-folder email syncs
    /// ([`Engine::sync_folder_email_streamed`]) **concurrently**: the folder-list scope
    /// is shared, so syncing it once up front lets the independent per-folder email
    /// scopes proceed in parallel without contending over it.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this account's
    /// folder-list scope, or [`ApiError::Sync`] if the provider fetch fails or the
    /// store rejects the apply.
    pub async fn sync_mailbox_list<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<SyncApplied, ApiError> {
        sync_mailbox_list(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Streams **only** the mail of the single folder `provider` is bound to, skipping
    /// the mailbox-list step — the per-folder counterpart of
    /// [`Engine::sync_mail_streamed`]. A host runs [`Engine::sync_mailbox_list`] once,
    /// then calls this for each folder provider **concurrently** (distinct mailbox
    /// scopes never contend), each reporting [`SyncCommit`](engine_sync::SyncCommit)s to
    /// `observer` after every committed chunk — the exact rows that changed, so a host
    /// splices its view without re-querying. `tuning` sets the depth window and
    /// decouples fetch batching from commit granularity
    /// ([`StreamTuning`](engine_sync::StreamTuning)); a cold backfill checkpoints per
    /// chunk, so a kill resumes where it stopped.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this folder's mail
    /// scope, or [`ApiError::Sync`] if the provider fetch fails or the store rejects an
    /// apply.
    pub async fn sync_folder_email_streamed<P: Provider, O: SyncObserver>(
        &self,
        provider: &P,
        account: &AccountId,
        tuning: StreamTuning,
        observer: &O,
    ) -> Result<SyncApplied, ApiError> {
        sync_email_streamed(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            tuning,
            observer,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Syncs one account's mail like [`Engine::sync_mail`], but **streams** the email
    /// scope: each chunk of messages commits as it arrives — so a host can render
    /// recent mail and live "downloaded Y of X" feedback before the whole sync
    /// finishes — reporting a [`SyncCommit`](engine_sync::SyncCommit) (progress **and**
    /// the exact upserted/removed rows) to `observer` after every committed chunk. A
    /// cold backfill checkpoints its cursor per chunk, so a mid-stream crash resumes
    /// from where it stopped rather than restarting. `tuning`
    /// ([`StreamTuning`](engine_sync::StreamTuning)) sets the depth window and decouples
    /// the fetch batch (round trips) from the chunk size (commit granularity).
    /// `observer` must be cheap and non-blocking (record into a snapshot, push onto a
    /// channel); a closure works via the blanket `SyncObserver` impl.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds the mail scope, or
    /// [`ApiError::Sync`] if the provider fetch fails or the store rejects an apply.
    pub async fn sync_mail_streamed<P: Provider, O: SyncObserver>(
        &self,
        provider: &P,
        account: &AccountId,
        tuning: StreamTuning,
        observer: &O,
    ) -> Result<MailSyncReport, ApiError> {
        sync_mail_streamed(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            tuning,
            observer,
        )
        .await
        .map_err(map_sync_error)
    }
}
