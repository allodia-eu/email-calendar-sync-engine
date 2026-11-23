//! Provider-driven sync, cache-reset/vacuum maintenance, and the streaming and
//! per-folder sync methods on `Engine`. How much mail the store keeps is `mail_depth`.

use engine_core::{
    ids::{AccountId, CalendarId},
    time::TimeZoneId,
};
use engine_provider::{ContactsProvider, Provider};
use engine_recurrence::Horizon;
use engine_store::Store;
use engine_sync::{
    CalendarReach, CalendarSyncReport, ContactSourceReport, ContactSyncReport, EventSyncReport,
    HorizonExpansion, MailSyncReport, PeopleRebuildReport, StreamTuning, SyncObserver,
    ThreadRebuildReport, expand_calendar_horizon, reach_calendar, rebuild_thread_index,
    reconcile_calendar_events, refresh_folders, sync_address_books, sync_calendar,
    sync_contact_cards, sync_contacts, sync_mail,
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
    /// whatever the account has — one per folder wherever the protocol's mail delta is
    /// per-folder (IMAP, and **Graph**, whose `email_scope` names a `GraphFolder`), a single
    /// element where one provider serves the account (JMAP, Gmail). The engine bounds how many
    /// run at once, puts the Inbox first, and runs the account-level store steps once rather
    /// than once per folder.
    ///
    /// That distinction is not cosmetic: a 56-folder Graph account passes **56 providers**
    /// here, every one of them speaking to a mailbox the server rate-limits as a *single*
    /// thing. How many requests they may make between them is not this bound — see
    /// `engine_http::RequestGate`, which a host holds one of per account.
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
    /// re-snapshot. A sync running meanwhile is stopped at its next commit rather than
    /// writing its cursor back. The same clear happens automatically when the engine's
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

    /// Syncs one account's calendars from `provider`, which serves every one of them: the
    /// calendar list first, then each calendar's events, concurrently, expanding each event's
    /// occurrences before the commit (`calendar-semantics.md`). `horizon` and `host_zone` seed
    /// the expansion window only on the account's first sync; after that the window the store
    /// holds wins ([`expand_horizon`](Engine::expand_horizon) moves it).
    ///
    /// A calendar whose listing tag has not moved since its last sync is skipped without a
    /// request. Each event scope's outcome is reported on its own in
    /// [`CalendarSyncReport::scopes`]: one calendar failing, or held by another pass, leaves
    /// the rest synced.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Busy`] if another sync already holds this account's calendar
    /// list, or [`ApiError::Sync`] if the list cannot be fetched or applied.
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

    /// Re-reads the **events** of `calendar`'s scope through the provider's delta and commits
    /// the result: the calendar list is neither fetched nor claimed, and no other calendar's
    /// scope is touched.
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
    /// Returns [`ApiError::Busy`] if another sync already holds the calendar's event scope,
    /// or [`ApiError::Sync`] if the provider fetch fails, the store rejects the apply, or the
    /// calendar has never been synced (there is then no window to expand over —
    /// [`sync_calendar`](Engine::sync_calendar) first).
    pub async fn reconcile_calendar_events<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        calendar: &CalendarId,
    ) -> Result<EventSyncReport, ApiError> {
        reconcile_calendar_events(
            provider,
            &self.store,
            account,
            calendar,
            worker(),
            LEASE_TTL,
        )
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
    /// A calendar another pass holds is **waited for**, not reported busy: that pass expands
    /// only what it changes, over the window it found, so nothing else would widen it. The wait
    /// ends when the pass does (or its lease lapses, if it died); drop the future to stop
    /// waiting. Expanding only what the store holds, it shows no event a windowed transport
    /// never fetched: [`reach_calendar`](Engine::reach_calendar) fetches those.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the store rejects the apply. An event whose recurrence the
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

    /// Makes the store hold every event of `account`'s calendars that falls in `range`, and keeps
    /// them current from then on.
    ///
    /// Most transports fetch a calendar whole, and for them this asks the server nothing. A
    /// **windowed** one
    /// ([`Capabilities::windowed_events`](engine_provider::Capabilities::windowed_events):
    /// Graph, Google) fetches only inside the window its cursor was opened over, the host's
    /// horizon at the first sync, and the store records that window. A range inside it is
    /// answered without a request; a range past it reopens the calendar's cursor over the union
    /// of the two, as a fresh snapshot, so the events it brings are kept current by every sync
    /// after. A range is never fetched twice: a second call for the same range waits for the
    /// first, then finds it covered. A calendar another pass holds is waited for, not reported
    /// busy.
    ///
    /// It fetches; it does not widen what is **shown**. A host paging its grid past the horizon
    /// calls this with the range it is about to show and [`expand_horizon`](Engine::expand_horizon)
    /// with its new horizon, in either order: each fetched event is expanded over the window the
    /// store holds. Each reopen re-reads the whole union, so ask in coarse steps, as for
    /// `expand_horizon`.
    ///
    /// While it runs, reads answer from the store as it was; the fetched events land in one
    /// commit per calendar. A calendar that fails is reported in
    /// [`CalendarReach::failures`] and records nothing, so asking again fetches it again.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the stored calendar list cannot be read. Everything after
    /// that is reported per calendar, one no [`sync_calendar`](Engine::sync_calendar) has
    /// reached as [`SyncError::NoExpansionWindow`](engine_sync::SyncError::NoExpansionWindow).
    pub async fn reach_calendar<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        range: Horizon,
    ) -> Result<CalendarReach, ApiError> {
        reach_calendar(provider, &self.store, account, worker(), LEASE_TTL, range)
            .await
            .map_err(map_sync_error)
    }
}
