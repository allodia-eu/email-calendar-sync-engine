//! Provider-driven sync, cache-reset/vacuum maintenance, and the streaming and
//! per-folder sync methods on `Engine`.

use engine_core::{ids::AccountId, sync::SearchDomain, time::TimeZoneId};
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::SyncApplied;
use engine_sync::{
    CalendarSyncReport, MailSyncReport, ProgressSink, ThreadDeriveReport, derive_mail_threads,
    sync_calendar, sync_email_streamed, sync_mail, sync_mail_streamed, sync_mailbox_list,
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
    /// scopes never contend), each reporting [`SyncProgress`](engine_sync::SyncProgress)
    /// to `progress` after every committed page. Only the final page advances the
    /// folder's cursor, so a mid-stream crash re-runs the pass idempotently.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this folder's mail
    /// scope, or [`ApiError::Sync`] if the provider fetch fails or the store rejects an
    /// apply.
    pub async fn sync_folder_email_streamed<P: Provider, K: ProgressSink>(
        &self,
        provider: &P,
        account: &AccountId,
        page_limit: usize,
        progress: &K,
    ) -> Result<SyncApplied, ApiError> {
        sync_email_streamed(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            page_limit,
            progress,
        )
        .await
        .map_err(map_sync_error)
    }

    /// Syncs one account's mail like [`Engine::sync_mail`], but **streams** the email
    /// scope: each page of messages commits as it arrives — so a host can render
    /// recent mail and live "downloaded Y of X" feedback before the whole sync
    /// finishes — reporting [`SyncProgress`](engine_sync::SyncProgress) to `progress`
    /// after every committed page. Only the final page advances the cursor, so a
    /// mid-stream crash re-runs the pass idempotently. `page_limit` bounds each page
    /// (`0` is the provider's maximum). `progress` must be cheap and non-blocking
    /// (push onto a channel); a closure works via the blanket `ProgressSink` impl.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds the mail scope, or
    /// [`ApiError::Sync`] if the provider fetch fails or the store rejects an apply.
    pub async fn sync_mail_streamed<P: Provider, K: ProgressSink>(
        &self,
        provider: &P,
        account: &AccountId,
        page_limit: usize,
        progress: &K,
    ) -> Result<MailSyncReport, ApiError> {
        sync_mail_streamed(
            provider,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            page_limit,
            progress,
        )
        .await
        .map_err(map_sync_error)
    }
}
