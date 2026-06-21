//! The [`Engine`]: a host's entry point to one account store.

use core::time::Duration;
use std::path::Path;

use engine_core::ids::AccountId;
use engine_core::time::TimeZoneId;
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{StoreError, WorkerId};
use engine_sync::{CalendarSyncReport, MailSyncReport, SyncError, sync_calendar, sync_mail};
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
