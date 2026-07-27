//! The [`Engine`]: a host's entry point to one account store.
//!
//! The methods are grouped across sibling modules to keep each file small:
//! provider-driven sync and cache maintenance (`sync`), the read and search surface
//! (`reads`), the outbox-mediated mail writes (`writes`), and the calendar writes, which
//! additionally reconcile the store to the server's copy (`calendar_writes`). This module
//! holds the type itself, its lifecycle constructors, the shared per-domain scope helper,
//! and the sync-error mapping every group reuses.

use core::time::Duration;
use std::path::Path;

use engine_core::{
    ids::AccountId,
    sync::{SearchDomain, SyncScope},
};
use engine_store::{StoreError, StoreRead, WorkerId};
use engine_sync::SyncError;
use store_sqlite::SqliteStore;

use crate::{ApiError, clock::SystemClock};

mod calendar_writes;
mod contact_photo;
mod contact_query;
mod contacts;
mod reads;
mod sync;
mod writes;

pub use calendar_writes::{CalendarDelete, CalendarWrite, Reconciled};
pub use contacts::{
    ContactDelete, ContactReconciled, ContactWrite, PeoplePage, PeopleQuery, RecipientSuggestions,
};

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
/// this one facade rather than wiring the engine crates themselves; the language
/// bindings are a follow-up slice.
///
/// `Engine` is `Send + Sync`; a host shares one across tasks as `Arc<Engine>`.
/// Concurrent syncs of *different* scopes proceed in parallel, but two concurrent
/// syncs of the *same* `(account, scope)` cannot both hold its lease — the loser
/// gets [`ApiError::Busy`] (it did nothing; retry once the other finishes) rather
/// than corrupting state.
#[derive(Debug)]
pub struct Engine {
    pub(crate) store: SqliteStore<SystemClock>,
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
pub(crate) fn map_sync_error(err: SyncError) -> ApiError {
    match err {
        SyncError::Store(StoreError::ScopeHeld) => ApiError::Busy,
        other => ApiError::Sync(other),
    }
}

/// Maps a payload-decode failure to a store error: the store wrote these objects, so a
/// failure to deserialize one back is store corruption, not host input.
fn decode_error(err: &serde_json::Error) -> ApiError {
    ApiError::Store(StoreError::Backend(err.to_string()))
}

#[cfg(test)]
mod tests {
    use engine_core::mail::Mailbox;

    use super::{ApiError, decode_error};

    #[test]
    fn decode_error_maps_a_corrupt_payload_to_a_store_error() {
        // A stored object that fails to deserialize is store corruption, not host
        // input, so the read methods surface it as `ApiError::Store`.
        let err = serde_json::from_str::<Mailbox>("not a mailbox").unwrap_err();
        assert!(matches!(decode_error(&err), ApiError::Store(_)));
    }
}
