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

mod blob;
mod convert;
mod derived_ops;
mod migrations;
mod outbox_ops;
mod schema;
mod scope_ops;
mod search_ops;
mod source_ops;

use core::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncState};
use engine_core::write::{PendingOp, PendingOpId, PendingOutcome};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{
    ApplyBatch, Clock, DerivedWrite, IndexRowCounts, LeaseRequest, LeasedPendingOp, OpLease,
    PendingOpState, Result, StorableObject, Store, StoreRead, SyncApplied, SyncClaim, SyncLease,
};

use crate::blob::BlobArea;
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
    /// The content-addressed blob area holding raw message sources beside (or, for
    /// in-memory stores, instead of) the database — large bytes never enter SQLite.
    blobs: Arc<BlobArea>,
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
        Self::configure(conn, clock, false, BlobArea::temporary()?)
    }

    /// Opens (creating if absent) a file-backed store at `path`, driven by
    /// `clock`. File databases run in WAL mode with a large mmap window.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] if the database cannot be
    /// opened or the schema cannot be created.
    pub fn open(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let path = path.as_ref();
        // Open the database first: an unusable path must fail here, before we would
        // otherwise create the blob directory (whose `create_dir_all` would mask the
        // bad path by materializing its missing parent).
        let conn = Connection::open(path).map_err(backend)?;
        let blobs = BlobArea::beside_db(path)?;
        Self::configure(conn, clock, true, blobs)
    }

    /// Applies the pragmas, migrates the schema to the latest version, and wraps
    /// the connection alongside its blob area.
    fn configure(mut conn: Connection, clock: C, on_disk: bool, blobs: BlobArea) -> Result<Self> {
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
        reconcile_normalizer_version(&conn, engine_store::NORMALIZER_VERSION)?;
        Ok(Self {
            clock,
            conn: Arc::new(Mutex::new(conn)),
            blobs: Arc::new(blobs),
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

    /// Clears every scope's sync cursor (and releases any held lease), so the next sync
    /// re-snapshots the account from scratch — re-fetching and **re-normalizing** every
    /// object. The durable outbox (queued sends) and the schema are untouched. Backs a
    /// host "reset / full refetch" action; the caller should sync afterwards to
    /// repopulate.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn reset_sync(&self) -> Result<()> {
        self.call(|conn| clear_sync_cursors(conn)).await
    }

    /// Clears one scope's sync cursor, so the next sync of that scope re-snapshots it
    /// from scratch. The targeted counterpart of [`reset_sync`](Self::reset_sync): a
    /// host reconciles a single domain (e.g. mail, to pick up flag/move/expunge changes
    /// an IMAP delta cannot detect without CONDSTORE — `imap-smtp.md`) without
    /// re-fetching the whole account.
    ///
    /// Unlike [`reset_sync`](Self::reset_sync) it **leaves any held lease intact**:
    /// this clear runs on every refresh, concurrently with fire-and-forget syncs, so it
    /// must not steal a live lease (it carries no fencing token to check, and clearing
    /// `lease_expiry` without bumping the generation would let a stolen-then-resumed
    /// worker commit its cursor back over the clear). An in-flight sync therefore keeps
    /// its lease; the cleared cursor takes effect on the next claim of the scope. The
    /// scope row, its objects, and the durable outbox are left in place.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn clear_scope_cursor(&self, scope: &SyncScope) -> Result<()> {
        let key = scope_key(scope);
        self.call(move |conn| clear_one_cursor(conn, &key)).await
    }
}

/// Clears every scope's cursor and lease so the next sync re-snapshots from scratch.
/// Leaves the scope rows (and their stable `scope_key`s) and objects in place — the
/// re-snapshot overwrites and tombstones them — so no object is orphaned.
fn clear_sync_cursors(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE sync_scope SET cursor = NULL, lease_expiry = NULL",
        [],
    )
    .map_err(backend)?;
    Ok(())
}

/// Clears one scope's cursor (by `scope_key`) so the next sync re-snapshots it. Leaves
/// `lease_expiry` and the fencing token untouched, so a concurrent in-flight sync's
/// lease is not stolen (see [`SqliteStore::clear_scope_cursor`]).
fn clear_one_cursor(conn: &Connection, scope_key: &str) -> Result<()> {
    conn.execute(
        "UPDATE sync_scope SET cursor = NULL WHERE scope_key = ?1",
        [scope_key],
    )
    .map_err(backend)?;
    Ok(())
}

/// On open, compares the stored `normalizer_version` to the build's `current`; on a
/// mismatch (including a pre-V4 database with no row) it clears the sync cursors so the
/// next sync re-normalizes everything, then records `current`. See
/// [`engine_store::NORMALIZER_VERSION`].
fn reconcile_normalizer_version(conn: &Connection, current: u32) -> Result<()> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'normalizer_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(backend)?;
    if stored.as_deref() == Some(current.to_string().as_str()) {
        return Ok(());
    }
    clear_sync_cursors(conn)?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('normalizer_version', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [current.to_string()],
    )
    .map_err(backend)?;
    Ok(())
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
    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>> {
        self.call(move |conn| scope_ops::account_scopes(conn, &account))
            .await
    }

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

    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>> {
        let key = scope_key(scope);
        self.call(move |conn| scope_ops::scope_objects(conn, &key))
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

    #[test]
    fn a_normalizer_version_change_clears_sync_cursors() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn).unwrap();

        // A synced scope carries a cursor; reconciling at the same version keeps it.
        super::reconcile_normalizer_version(&conn, 1).unwrap();
        conn.execute(
            "INSERT INTO sync_scope (scope_key, account, token, cursor) VALUES ('s', 'a', 1, 'c1')",
            [],
        )
        .unwrap();
        super::reconcile_normalizer_version(&conn, 1).unwrap();
        let cursor: Option<String> = conn
            .query_row(
                "SELECT cursor FROM sync_scope WHERE scope_key = 's'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            cursor.as_deref(),
            Some("c1"),
            "unchanged version keeps cursors"
        );

        // A bump clears the cursor, so the next sync re-snapshots + re-normalizes.
        super::reconcile_normalizer_version(&conn, 2).unwrap();
        let cursor: Option<String> = conn
            .query_row(
                "SELECT cursor FROM sync_scope WHERE scope_key = 's'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cursor, None, "a version bump clears cursors");
    }

    #[test]
    fn clear_one_cursor_clears_the_cursor_but_keeps_a_held_lease() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::migrations::migrate(&mut conn).unwrap();

        // A scope mid-sync: a cursor plus a live lease (a fencing token and a future
        // expiry). The per-scope clear runs concurrently with such syncs, so unlike
        // reset_sync it must clear ONLY the cursor — stealing the lease would let the
        // in-flight worker commit its cursor back over the clear.
        conn.execute(
            "INSERT INTO sync_scope (scope_key, account, token, cursor, lease_expiry) \
             VALUES ('s', 'a', 5, 'c1', '2099-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        super::clear_one_cursor(&conn, "s").unwrap();

        let (cursor, token, lease): (Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT cursor, token, lease_expiry FROM sync_scope WHERE scope_key = 's'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            cursor, None,
            "the cursor is cleared so the next sync snapshots"
        );
        assert_eq!(token, 5, "the fencing token is untouched");
        assert_eq!(
            lease.as_deref(),
            Some("2099-01-01T00:00:00Z"),
            "a live lease is NOT stolen (the contrast with reset_sync)"
        );
    }
}
