//! The [`Engine`]: a host's entry point to one account store.

use core::time::Duration;
use std::path::Path;

use engine_core::calendar::{Calendar, Event};
use engine_core::ids::AccountId;
use engine_core::mail::{Mailbox, Message};
use engine_core::sync::{ObjectKind, SearchDomain, SyncScope};
use engine_core::time::TimeZoneId;
use engine_core::write::PendingOpId;
use engine_provider::{Draft, Provider};
use engine_recurrence::Horizon;
use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{PendingOpState, StoreError, StoreRead, WorkerId};
use engine_sync::{
    CalendarSyncReport, MailSyncReport, ProgressSink, SubmitOutcome, SyncError, submit_mail,
    sync_calendar, sync_mail, sync_mail_streamed,
};
use serde_json::Value;
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

    /// Lists one account's mailboxes (folders/labels) — the synced mail collections
    /// across the account's mailbox scopes — for the host's folder sidebar.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn mailboxes(&self, account: &AccountId) -> Result<Vec<Mailbox>, ApiError> {
        let mut mailboxes = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Mailbox).await? {
            mailboxes.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(mailboxes)
    }

    /// Lists one account's messages — the synced mail objects (envelope metadata;
    /// bodies are fetched on demand) across the account's mail scopes. For the message
    /// list; pair with [`Engine::search_mail`] for filtered or ranked views.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn messages(&self, account: &AccountId) -> Result<Vec<Message>, ApiError> {
        let mut messages = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Message).await? {
            messages.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(messages)
    }

    /// Lists one account's calendars (collections) — the synced calendar containers
    /// across the account's calendar scopes — for the host's calendar sidebar.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn calendars(&self, account: &AccountId) -> Result<Vec<Calendar>, ApiError> {
        let mut calendars = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Calendar).await? {
            calendars.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(calendars)
    }

    /// Lists one account's events — the synced calendar event objects (the projected
    /// envelope; recurrence materializes into occurrences in the store) across the
    /// account's calendar scopes. For the agenda/event list; pair with
    /// [`Engine::search_calendar`] for filtered or ranked views.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] on a backend failure.
    pub async fn events(&self, account: &AccountId) -> Result<Vec<Event>, ApiError> {
        let mut events = Vec::new();
        for payload in self.objects_of(account, ObjectKind::Event).await? {
            events.push(serde_json::from_value(payload).map_err(|err| decode_error(&err))?);
        }
        Ok(events)
    }

    /// The normalized payload of every object of `kind` across the account's scopes,
    /// enumerated and filtered by [`SyncScope::object_kind`] — so the facade never
    /// hard-codes or branches on which scopes a provider uses. One batch read per scope
    /// (no per-key round trip).
    async fn objects_of(
        &self,
        account: &AccountId,
        kind: ObjectKind,
    ) -> Result<Vec<Value>, ApiError> {
        let scopes = self.store.account_scopes(account.clone()).await?;
        let mut payloads = Vec::new();
        for scope in scopes
            .into_iter()
            .filter(|scope| scope.object_kind() == Some(kind))
        {
            payloads.extend(
                self.store
                    .scope_objects(&scope)
                    .await?
                    .into_iter()
                    .map(|(_key, payload)| payload),
            );
        }
        Ok(payloads)
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

/// Maps a payload-decode failure to a store error: the store wrote these objects, so a
/// failure to deserialize one back is store corruption, not host input.
fn decode_error(err: &serde_json::Error) -> ApiError {
    ApiError::Store(StoreError::Backend(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{ApiError, Mailbox, decode_error};

    #[test]
    fn decode_error_maps_a_corrupt_payload_to_a_store_error() {
        // A stored object that fails to deserialize is store corruption, not host
        // input, so the read methods surface it as `ApiError::Store`.
        let err = serde_json::from_str::<Mailbox>("not a mailbox").unwrap_err();
        assert!(matches!(decode_error(&err), ApiError::Store(_)));
    }
}
