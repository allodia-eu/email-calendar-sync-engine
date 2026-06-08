//! `store-sqlite` — the durable SQLite backend for the PIM sync engine.
//!
//! [`SqliteStore`] implements the `engine-store` [`Store`] and [`StoreRead`]
//! contracts over SQLite, so it passes the shared `engine_store::contract` suite
//! the in-memory reference store passes. It is the first persistent store; other
//! backends are host adapters.
//!
//! Design (see `docs/agent-guidance/store-and-sync.md`):
//!
//! - **Mechanical.** The store writes the precomputed [`DerivedWrite`] and the
//!   opaque serialized objects keyed by provider key; it performs no
//!   normalization, text extraction, or recurrence expansion.
//! - **Fenced.** Each scope and op carries a monotonic generation; a write is
//!   admitted only if its lease token still equals the stored generation,
//!   re-checked inside the write transaction.
//! - **Encryption-agnostic.** At-rest protection is a *construction* detail (plain
//!   SQLite over OS file encryption by default; SQLCipher is an opt-in build), so
//!   the contract holds either way. Credentials never enter this store.
//! - **Async over sync.** rusqlite is synchronous; every call runs on a blocking
//!   thread via [`tokio::task::spawn_blocking`] against one mutex-guarded
//!   connection (one connection per database — required for `:memory:`, where
//!   each connection is its own database).
//!
//! The FTS5 search index and the normalized structured-filter tables layer over
//! this base in migration `V2` (`schema.rs`); content-addressed blob storage is a
//! later sub-step.

mod convert;
mod derived_ops;
mod migrations;
mod outbox_ops;
mod schema;
mod scope_ops;
mod search_ops;

use core::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncState};
use engine_core::write::{PendingOp, PendingOpId, PendingOutcome};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{
    ApplyBatch, Clock, DerivedWrite, IndexRowCounts, LeaseRequest, LeasedPendingOp, OpLease,
    PendingOpState, Result, StorableObject, Store, StoreRead, SyncApplied, SyncClaim, SyncLease,
};

use crate::convert::{backend, expiry_after, scope_key};
use crate::scope_ops::OwnedUpdate;

/// The default mmap window for file-backed databases (256 MiB): fewer read
/// syscalls on the hot search path, so query cost tracks index size.
const MMAP_BYTES: i64 = 256 * 1024 * 1024;

/// A SQLite-backed [`Store`] + [`StoreRead`], parameterized by an injected
/// [`Clock`] for lease-expiry control (a [`engine_store::ManualClock`] in tests,
/// a host clock in production).
///
/// All access goes through one connection behind a mutex; rusqlite work is
/// offloaded to a blocking thread so the async runtime is never blocked.
pub struct SqliteStore<C> {
    clock: C,
    conn: Arc<Mutex<Connection>>,
}

impl<C> fmt::Debug for SqliteStore<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redacted: the connection may map a file holding sensitive mail data.
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl<C: Clock> SqliteStore<C> {
    /// Opens an ephemeral in-memory store (one connection = one database), driven
    /// by `clock`. Each call is an isolated, empty database.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] if the database cannot be
    /// opened or the schema cannot be created.
    pub fn open_in_memory(clock: C) -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::configure(conn, clock, false)
    }

    /// Opens (creating if absent) a file-backed store at `path`, driven by
    /// `clock`. File databases run in WAL mode with a large mmap window.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] if the database cannot be
    /// opened or the schema cannot be created.
    pub fn open(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::configure(conn, clock, true)
    }

    /// Applies the pragmas, migrates the schema to the latest version, and wraps
    /// the connection.
    fn configure(mut conn: Connection, clock: C, on_disk: bool) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
            .map_err(backend)?;
        if on_disk {
            // execute_batch tolerates the rows journal_mode/mmap_size echo back.
            conn.execute_batch(&format!(
                "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA mmap_size = {MMAP_BYTES};"
            ))
            .map_err(backend)?;
        }
        migrations::migrate(&mut conn)?;
        Ok(Self {
            clock,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Runs `f` against the connection on a blocking thread.
    ///
    /// Serializes access through the mutex (a single connection is required for
    /// `:memory:` anyway); WAL concurrency for file databases is a later
    /// read-pool concern, not a contract concern.
    async fn call<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().expect("sqlite connection mutex poisoned");
            f(&mut guard)
        })
        .await
        .expect("sqlite blocking task panicked")
    }

    /// Searches mail across `scopes`, returning ranked hits and the answer's
    /// coverage. The query compiles to indexed structured filters plus an FTS5
    /// `bm25()` ranking; pass the account's mail scopes (search is per-account).
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn search_mail(
        &self,
        scopes: &[SyncScope],
        query: &MailQuery,
        limit: usize,
    ) -> Result<SearchResults> {
        let scope_keys: Vec<String> = scopes.iter().map(scope_key).collect();
        let scope_count = scopes.len();
        let query = query.clone();
        let ranked = self
            .call(move |conn| search_ops::search_mail(conn, &scope_keys, &query, limit))
            .await?;
        search_ops::assemble_results(ranked, scope_count)
    }

    /// Searches calendar events across `scopes`, returning ranked hits and
    /// coverage. Time-range (`before:`/`after:`) filters match materialized
    /// occurrences.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn search_calendar(
        &self,
        scopes: &[SyncScope],
        query: &CalendarQuery,
        limit: usize,
    ) -> Result<SearchResults> {
        let scope_keys: Vec<String> = scopes.iter().map(scope_key).collect();
        let scope_count = scopes.len();
        let query = query.clone();
        let ranked = self
            .call(move |conn| search_ops::search_calendar(conn, &scope_keys, &query, limit))
            .await?;
        search_ops::assemble_results(ranked, scope_count)
    }
}

#[async_trait]
impl<C: Clock> Store for SqliteStore<C> {
    async fn load_sync_state(
        &self,
        _account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>> {
        let key = scope_key(scope);
        self.call(move |conn| scope_ops::load_state(conn, &key))
            .await
    }

    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim> {
        let now = self.clock.now();
        let expiry = expiry_after(now, req.ttl)?;
        let key = scope_key(scope);
        let scope = scope.clone();
        let owner = req.owner;
        self.call(move |conn| scope_ops::claim(conn, account, scope, &key, owner, now, expiry))
            .await
    }

    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: StorableObject + Serialize + Send + Sync,
    {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        let update = OwnedUpdate::from_update(batch.update)?;
        let derived = batch.derived.clone();
        let reconcile = batch.reconcile.to_vec();
        // `None` (a streaming page) leaves the cursor unchanged.
        let next_state = batch.next_state.map(|s| s.as_str().to_owned());
        self.call(move |conn| {
            scope_ops::apply(
                conn,
                &key,
                token,
                &update,
                &derived,
                &reconcile,
                next_state.as_deref(),
            )
        })
        .await
    }

    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()> {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        let derived = derived.clone();
        self.call(move |conn| scope_ops::maintenance(conn, &key, token, &derived))
            .await
    }

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        self.call(move |conn| scope_ops::release(conn, &key, token))
            .await
    }

    async fn enqueue_pending_op(&self, account: AccountId, op: PendingOp) -> Result<PendingOpId> {
        self.call(move |conn| outbox_ops::enqueue(conn, &account, &op))
            .await
    }

    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        let now = self.clock.now();
        let expiry = expiry_after(now, req.ttl)?;
        let owner = req.owner;
        self.call(move |conn| outbox_ops::claim(conn, &account, &owner, now, expiry, limit))
            .await
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        let op_id = lease.op();
        let token = lease.token().get();
        self.call(move |conn| outbox_ops::mark(conn, op_id, token, &outcome))
            .await
    }
}

#[async_trait]
impl<C: Clock> StoreRead for SqliteStore<C> {
    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>> {
        let key = scope_key(scope);
        self.call(move |conn| scope_ops::object_keys(conn, &key))
            .await
    }

    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>> {
        let scope = scope_key(scope);
        let provider_key = key.as_str().to_owned();
        self.call(move |conn| scope_ops::object_payload(conn, &scope, &provider_key))
            .await
    }

    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>> {
        self.call(move |conn| outbox_ops::pending_op_state(conn, id))
            .await
    }

    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts> {
        let scope = scope_key(scope);
        let provider_key = key.as_str().to_owned();
        self.call(move |conn| derived_ops::index_row_counts(conn, &scope, &provider_key))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteStore;
    use engine_store::ManualClock;

    #[test]
    fn debug_is_redacted() {
        // The Debug form must not expose the connection (it may map sensitive data).
        let store = SqliteStore::open_in_memory(ManualClock::new(
            "2026-01-01T00:00:00Z".parse().expect("valid instant"),
        ))
        .expect("open");
        let rendered = format!("{store:?}");
        assert!(rendered.contains("SqliteStore"));
        assert!(rendered.contains(".."));
    }
}
