//! The [`Engine`]: a host's entry point to one account store.

use core::time::Duration;
use std::path::Path;

use engine_core::ids::AccountId;
use engine_core::sync::{SearchDomain, SyncScope};
use engine_core::time::TimeZoneId;
use engine_core::write::PendingOpId;
use engine_provider::{Draft, Provider};
use engine_recurrence::Horizon;
use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{PendingOpState, StoreError, StoreRead, WorkerId};
use engine_sync::{
    CalendarSyncReport, MailSyncReport, SubmitOutcome, SyncError, submit_mail, sync_calendar,
    sync_mail,
};
use store_sqlite::SqliteStore;

use crate::ApiError;
use crate::clock::SystemClock;

/// The worker identity this engine stamps on every lease it claims.
///
/// One engine instance is one logical writer; the fencing token (not this id)
/// serializes a suspended-then-resumed worker against itself (`store-and-sync.md`).
/// Distinct-per-device identities for a multi-writer account are a later,
/// host-configured slice.
const WORKER: &str = "engine-api";

/// How long a sync lease stays valid before another worker may re-claim its scope.
///
/// Generous enough to cover a slow first sync over a mobile network; the loop
/// re-claims and recomputes if the lease is ever superseded mid-flight, so this is
/// a safety bound, not a deadline.
const LEASE_TTL: Duration = Duration::from_mins(5);

/// The stable, host-facing entry point to the engine (`north-star.md`).
///
/// An `Engine` owns one durable [`SqliteStore`] — the first store; other backends
/// are host adapters — driven by the host wall clock. Hosts sync accounts through
/// this one facade rather than wiring the engine crates themselves; search, the
/// write/outbox surface, and the language bindings are follow-up slices.
///
/// `Engine` is `Send + Sync`; a host shares one across tasks as `Arc<Engine>`.
/// Concurrent syncs of *different* scopes proceed in parallel, but two concurrent
/// syncs of the *same* `(account, scope)` cannot both hold its lease — the loser
/// gets [`ApiError::Busy`] (it did nothing; retry once the other finishes) rather
/// than corrupting state.
#[derive(Debug)]
pub struct Engine {
    store: SqliteStore<SystemClock>,
}

impl Engine {
    /// Opens (creating if absent) a file-backed engine at `path`, migrated to the
    /// latest schema and driven by the host wall clock.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] if the database cannot be opened or migrated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApiError> {
        Ok(Self {
            store: SqliteStore::open(path, SystemClock)?,
        })
    }

    /// Opens an ephemeral in-memory engine (one connection is one database), for
    /// tests and short-lived hosts.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] if the database cannot be initialized.
    pub fn open_in_memory() -> Result<Self, ApiError> {
        Ok(Self {
            store: SqliteStore::open_in_memory(SystemClock)?,
        })
    }

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

    /// Searches one account's mail with the textual DSL (`from:a subject:"q report"
    /// before:2026-01-01`), returning ranked object keys and the answer's coverage.
    /// Runs over the account's mail scopes, enumerated from the store rather than
    /// hard-coded, so the facade stays provider-agnostic.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Query`] if `query` is malformed, or [`ApiError::Store`]
    /// on a backend failure.
    pub async fn search_mail(
        &self,
        account: &AccountId,
        query: &str,
        limit: usize,
    ) -> Result<SearchResults, ApiError> {
        let query = MailQuery::parse(query)?;
        let scopes = self.scopes_in(account, SearchDomain::Mail).await?;
        Ok(self.store.search_mail(&scopes, &query, limit).await?)
    }

    /// Searches one account's calendar events with the textual DSL (`calendar:work
    /// attendee:a@x after:2026-06-01`); `before:`/`after:` match the materialized
    /// occurrences, not just the master event (`calendar-semantics.md`).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Query`] if `query` is malformed, or [`ApiError::Store`]
    /// on a backend failure.
    pub async fn search_calendar(
        &self,
        account: &AccountId,
        query: &str,
        limit: usize,
    ) -> Result<SearchResults, ApiError> {
        let query = CalendarQuery::parse(query)?;
        let scopes = self.scopes_in(account, SearchDomain::Calendar).await?;
        Ok(self.store.search_calendar(&scopes, &query, limit).await?)
    }

    /// Submits `draft` for one account through the durable outbox: the draft is
    /// recorded as a pending op (idempotent by its `Message-ID`) **before** the
    /// provider send, so a crash or an ambiguous failure never loses or double-sends
    /// it (`north-star.md` Write Contract). Returns the sent message's key, its
    /// `Message-ID`, and the op id — pollable via [`Engine::pending_op_state`].
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Sync`] if the send fails: the op is first recorded
    /// `Failed` (with the failure class), or `NeedsConfirmation` for an ambiguous
    /// post-`DATA` SMTP loss — the outbox never blind-retries — and the error then
    /// returns. A store failure also surfaces as [`ApiError::Sync`].
    pub async fn submit_mail<P: Provider>(
        &self,
        provider: &P,
        account: &AccountId,
        draft: &Draft,
    ) -> Result<SubmitOutcome, ApiError> {
        submit_mail(provider, &self.store, account, worker(), LEASE_TTL, draft)
            .await
            .map_err(map_sync_error)
    }

    /// The current lifecycle state of a pending outbox op — e.g. the one a
    /// [`submit_mail`](Self::submit_mail) returned — or `None` if no such op exists.
    /// A lease-free read, safe to poll for write progress and confirmation state.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn pending_op_state(
        &self,
        op: PendingOpId,
    ) -> Result<Option<PendingOpState>, ApiError> {
        Ok(self.store.pending_op_state(op).await?)
    }

    /// The account's scopes in one search domain: every scope the store knows for
    /// the account, filtered by [`SyncScope::search_domain`]. Enumerating instead
    /// of hard-coding keeps the facade from branching on protocol or naming a
    /// provider's scopes.
    async fn scopes_in(
        &self,
        account: &AccountId,
        domain: SearchDomain,
    ) -> Result<Vec<SyncScope>, ApiError> {
        Ok(self
            .store
            .account_scopes(account.clone())
            .await?
            .into_iter()
            .filter(|scope| scope.search_domain() == Some(domain))
            .collect())
    }
}

/// The lease owner identity this engine stamps (see [`WORKER`]).
fn worker() -> WorkerId {
    WorkerId::new(WORKER)
}

/// Translates a sync failure into an [`ApiError`], splitting the benign
/// scope-contention race out as [`ApiError::Busy`]. A concurrent sync of the same
/// `(account, scope)` makes the store return the retryable [`StoreError::ScopeHeld`];
/// the sync loop surfaces it rather than waiting for the live lease, so the facade
/// reports it as `Busy` — distinct from a real failure a host should not retry.
fn map_sync_error(err: SyncError) -> ApiError {
    match err {
        SyncError::Store(StoreError::ScopeHeld) => ApiError::Busy,
        other => ApiError::Sync(other),
    }
}
