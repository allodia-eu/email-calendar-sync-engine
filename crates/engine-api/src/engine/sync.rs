//! Provider-driven sync, cache-reset/vacuum maintenance, and the streaming and
//! per-folder sync methods on `Engine`.

use engine_core::{
    ids::AccountId,
    sync::{SearchDomain, SyncWindow},
    time::TimeZoneId,
};
use engine_provider::{ContactsProvider, Provider};
use engine_recurrence::Horizon;
use engine_store::{MessageSourceCache, PruneReport, SourcesDropped, Store, SweepReport};
use engine_sync::{
    CalendarSyncReport, ContactSourceReport, ContactSyncReport, EventSyncReport, HorizonExpansion,
    MailSyncReport, PeopleRebuildReport, StreamTuning, SyncObserver, ThreadRebuildReport,
    expand_calendar_horizon, rebuild_thread_index, reconcile_calendar_events, refresh_folders,
    sync_address_books, sync_calendar, sync_contact_cards, sync_contacts, sync_mail,
};

use super::{LEASE_TTL, map_sync_error, worker};
use crate::{ApiError, Engine};

impl Engine {
    /// Discovers contact sources/address books once for an account.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] for provider, store, or scope-lease failure.
    pub async fn sync_address_books<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<ContactSourceReport, ApiError> {
        sync_address_books(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Syncs cards for one source-bound adapter and rebuilds unified people.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] for provider, store, lease, or people-rebuild failure.
    pub async fn sync_contact_cards<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<(ContactSourceReport, PeopleRebuildReport), ApiError> {
        sync_contact_cards(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Runs contact discovery, card sync, and people derivation.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError`] under the same rules as the source-level methods.
    pub async fn sync_contacts<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<ContactSyncReport, ApiError> {
        sync_contacts(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Syncs one account's mail: the folder list once, then every folder, each through the
    /// claim → fetch → derive → apply → release cycle with `StaleLease` recovery
    /// (`store-and-sync.md`).
    ///
    /// **This is the only way to sync mail, and the engine drives the fan-out.** `providers` is
    /// whatever the account has — one per folder where the protocol binds a connection to a
    /// mailbox (IMAP), a single element where one provider serves the account (JMAP, Graph,
    /// Gmail). The engine bounds how many run at once, puts the Inbox first, and runs the
    /// account-level store steps once rather than once per folder.
    ///
    /// `observer` receives every committed chunk plus the pass's lifecycle, so a host can splice
    /// its list and show which account is syncing without polling.
    ///
    /// Returns no `Result`: a partial failure is ordinary, and
    /// [`MailSyncReport`](engine_sync::MailSyncReport) reports each scope's outcome separately so
    /// a caller can tell an outage from an expired sign-in from a scope another pass is holding.
    pub async fn sync_mail<P: Provider, O: SyncObserver>(
        &self,
        providers: &[P],
        account: &AccountId,
        tuning: StreamTuning,
        observer: &O,
    ) -> MailSyncReport {
        sync_mail(
            providers,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            tuning,
            observer,
        )
        .await
    }

    /// Refreshes exactly the folders given, discovering nothing.
    ///
    /// The targeted counterpart of [`Engine::sync_mail`], for a caller that already knows which
    /// folder changed — an IMAP `IDLE` push, a webhook, a folder the user just opened. It runs no
    /// account-level work: no folder-list sync, no thread-index repair, no recipient backfill, no
    /// coverage record. [`MailSyncReport::mailboxes`](engine_sync::MailSyncReport) is `None`,
    /// which is neither success nor failure — nothing was asked of the server, so a caller
    /// weighing reachability must read the folders instead.
    ///
    /// **Discovery is most of what a targeted refresh would otherwise pay.** Measured on a
    /// steady-state single-folder pass against a live server, the folder list was 57% of the work
    /// where `LIST-STATUS` is available and 86% where it is not — and in round trips, which is
    /// what a remote server actually charges for, a server without `LIST-STATUS` is asked for a
    /// `STATUS` per folder: one extra trip becomes fourteen on a thirteen-folder account.
    ///
    /// Use [`Engine::sync_mail`] for a periodic or user-triggered pass; new account-level work
    /// goes there and only there.
    pub async fn refresh_folders<P: Provider, O: SyncObserver>(
        &self,
        providers: &[P],
        account: &AccountId,
        tuning: StreamTuning,
        observer: &O,
    ) -> MailSyncReport {
        refresh_folders(
            providers,
            &self.store,
            account,
            worker(),
            LEASE_TTL,
            tuning,
            observer,
        )
        .await
    }

    /// Rebuilds the account's derived thread ids and the message-id graph behind them from the
    /// stored payloads, grouping messages across folders by their
    /// `Message-ID`/`In-Reply-To`/`References` headers (so a sent reply and its received original
    /// share a thread). A no-op for providers that assign thread ids themselves.
    ///
    /// **Nothing is required to call this.** A sync threads what it applies inside the same
    /// transaction, and repairs the one shape an arrival cannot — a message left in the graph with
    /// no thread by the migration that introduced it — by asking the store and rebuilding when the
    /// answer is yes — [`Engine::sync_mail`] does it once per pass, before any folder runs. So an
    /// ordinary pass needs nothing here, and calling it
    /// anyway would re-read every payload in the account to confirm an answer already written.
    /// This stays public for the case a support answer calls for: an index someone has reason to
    /// doubt. It writes no thread id over mail that is already right.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if a sync already holds a mail scope, or
    /// [`ApiError::Sync`] if the store rejects the apply.
    pub async fn rebuild_thread_index(
        &self,
        account: &AccountId,
    ) -> Result<ThreadRebuildReport, ApiError> {
        rebuild_thread_index(&self.store, account, worker(), LEASE_TTL)
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

    /// Abandons sync leases left behind by an abruptly terminated host process,
    /// preserving the last committed cursors so resumable syncs continue from their
    /// checkpoints instead of waiting for the lease TTL or clearing state.
    ///
    /// Call this only when the host knows older workers using this store are gone
    /// (typically once during process startup). It clears held scope leases and
    /// bumps their fencing tokens, so any abandoned worker that later tries to
    /// commit is rejected as stale. It is not a normal in-process `Busy` recovery
    /// path.
    ///
    /// Returns the number of held scope leases abandoned.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn abandon_sync_leases(&self) -> Result<usize, ApiError> {
        Ok(self.store.abandon_sync_leases().await?)
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
        window: SyncWindow,
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

    /// Re-reads one account's **events** through the provider's delta and commits the
    /// result: the calendar containers are neither fetched nor claimed.
    ///
    /// It re-expands whatever it changed over the window the store already holds, so — like
    /// a write — it can neither widen nor narrow what the host has expanded.
    ///
    /// Every facade calendar write already runs this for itself
    /// ([`create_calendar_event`](Engine::create_calendar_event) and friends), so a host
    /// rarely calls it directly. Two cases still want it:
    ///
    /// - **Recovery.** A write whose reconcile came back [`Reconciled::Busy`](crate::Reconciled) or
    ///   [`Failed`](crate::Reconciled::Failed) left the store holding the pre-write copy. This
    ///   brings it up to date without a full [`sync_calendar`](Engine::sync_calendar).
    /// - **Batching.** A caller driving the *low-level* `engine_sync` drivers (which do not
    ///   reconcile) runs one of these after its last write rather than one per write. The facade
    ///   writes here always reconcile, so N of them cost N deltas.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this account's event
    /// scope, or [`ApiError::Sync`] if the provider fetch fails, the store rejects the
    /// apply, or the account's calendars have never been synced (there is then no window to
    /// expand over — [`sync_calendar`](Engine::sync_calendar) first).
    pub async fn reconcile_calendar_events<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
    ) -> Result<EventSyncReport, ApiError> {
        reconcile_calendar_events(provider, &self.store, account, worker(), LEASE_TTL)
            .await
            .map_err(map_sync_error)
    }

    /// Re-expands one account's **already-synced** events over `horizon`, resolving
    /// floating times through `host_zone`, and commits the fresh occurrences. No
    /// network.
    ///
    /// A host must call this before reading a window that no
    /// [`sync_calendar`](Engine::sync_calendar) has materialized — **re-syncing will
    /// not do it.** A sync expands only the objects its delta *changed*, so a provider
    /// that reports "nothing changed" (the normal case) derives no occurrences at all,
    /// and [`Engine::occurrences_in`] over the newly-visible range returns nothing,
    /// forever. A calendar grid paging into next year would render a confidently empty
    /// week.
    ///
    /// Also the path for a **display-zone or tzdata change**: a floating event's stored
    /// instant is only correct for the zone it was expanded under, so a zone change
    /// without a re-expansion silently shifts every floating event by the zone offset.
    ///
    /// It re-expands every stored event on every call, so widen in coarse chunks
    /// against a persisted watermark rather than calling it per page.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync holds a calendar scope, or
    /// [`ApiError::Sync`] if the store rejects the apply. An event whose recurrence the
    /// expander cannot handle is **not** an error — it is reported in
    /// [`HorizonExpansion::unexpandable`], so one unsupported rule never stops the rest
    /// of the calendar from materializing.
    pub async fn expand_horizon(
        &self,
        account: &AccountId,
        horizon: Horizon,
        host_zone: &TimeZoneId,
    ) -> Result<HorizonExpansion, ApiError> {
        expand_calendar_horizon(
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
}
