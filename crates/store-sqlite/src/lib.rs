//! `store-sqlite` — the durable SQLite backend for the PIM sync engine.
//!
//! [`SqliteStore`] implements the `engine-store` [`Store`] and
//! [`StoreRead`](engine_store::StoreRead)
//! contracts over SQLite, so it passes the shared `engine_store::contract` suite
//! the in-memory reference store passes. It is the first persistent store; other
//! backends are host adapters.
//!
//! Design (see `docs/agent-guidance/store-and-sync.md`):
//!
//! - **Mechanical.** The store writes the precomputed [`DerivedWrite`] and the opaque serialized
//!   objects keyed by provider key; it performs no normalization, text extraction, or recurrence
//!   expansion.
//! - **Fenced.** Each scope and op carries a monotonic generation; a write is admitted only if its
//!   lease token still equals the stored generation, re-checked inside the write transaction.
//! - **Encryption-agnostic.** At-rest protection is a *construction* detail (plain SQLite over OS
//!   file encryption by default; SQLCipher is an opt-in build), so the contract holds either way.
//!   Credentials never enter this store.
//! - **Async over sync.** rusqlite is synchronous; every call runs on a blocking thread via
//!   [`tokio::task::spawn_blocking`]. A file database splits into one writer connection and a pool
//!   of `query_only` readers (`pool.rs`), so a committing sync and a list read no longer queue
//!   behind each other; an in-memory database keeps a single connection, because there each
//!   connection is its own database.
//!
//! The FTS5 search index and the normalized structured-filter tables layer over
//! this base in migration `V2` (`schema.rs`). On-demand message content (`V5`) splits
//! by *text vs bytes*: the raw message bytes live in a content-addressed filesystem
//! blob area (`blob.rs`) — never in SQLite — while the extracted body text and its
//! own lease-free FTS index live in `message_body`/`message_body_fts` (`source_ops.rs`).

mod backfill;
mod blob;
mod contact_ops;
mod contact_store;
mod convert;
mod derived_ops;
mod mail_ops;
mod migrations;
mod options;
mod outbox_ops;
mod photo_ops;
mod pool;
mod prune;
mod purge;
mod read;
mod schema;
mod scope_ops;
mod search_ops;
mod source_ops;
mod sql;
mod sweep;
mod window_ops;

use core::fmt;
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    sync::{ObjectKind, SyncObject, SyncScope, SyncState},
    time::ExpansionWindow,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_search::{CalendarQuery, MailQuery, SearchResults};
use engine_store::{
    ApplyBatch, Clock, DerivedWrite, LeaseRequest, LeasedPendingOp, OpLease, Result, SchemaStatus,
    Store, SyncApplied, SyncClaim, SyncLease,
};
pub use options::{FtsTokenizer, OpenOptions};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::{
    blob::BlobArea,
    convert::{backend, expiry_after, scope_key},
    pool::Pool,
    scope_ops::OwnedUpdate,
};

/// A SQLite-backed [`Store`] + [`StoreRead`](engine_store::StoreRead), parameterized by an injected
/// [`Clock`] for lease-expiry control (a [`engine_store::ManualClock`] in tests,
/// a host clock in production).
///
/// Writes take the pool's single writer connection; reads take a free reader (the
/// writer itself, for an in-memory database). rusqlite work is offloaded to a
/// blocking thread so the async runtime is never blocked.
pub struct SqliteStore<C> {
    clock: C,
    /// Where the schema stood after this store was opened, including what opening moved it
    /// from. Captured at open because that is the only moment the *previous* version exists.
    schema: SchemaStatus,
    pool: Arc<Pool>,
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
        Self::configure(conn, clock, None, BlobArea::temporary()?)
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
        Self::configure(conn, clock, Some(path), blobs)
    }

    /// Applies the pragmas, migrates the schema to the latest version, opens the
    /// reader pool, and wraps them alongside the blob area.
    ///
    /// `path` is `Some` exactly when the database is file-backed — which is what
    /// decides both the WAL pragmas and whether readers can be opened at all.
    fn configure(
        mut conn: Connection,
        clock: C,
        path: Option<&Path>,
        blobs: BlobArea,
    ) -> Result<Self> {
        pool::tune(&conn, path.is_some())?;
        let schema = migrations::migrate(&mut conn, FtsTokenizer::PorterUnicode61)?;
        reconcile_normalizer_version(&conn, engine_store::NORMALIZER_VERSION)?;
        // After the migration, so a reader never sees a schema mid-step.
        let readers = match path {
            Some(path) => pool::open_readers(path)?,
            None => Vec::new(),
        };
        Ok(Self {
            clock,
            schema,
            pool: Arc::new(Pool::new(conn, readers)),
            blobs: Arc::new(blobs),
        })
    }

    /// Runs `f` against the **writer** on a blocking thread. Every transaction and
    /// every pragma that changes the database goes here.
    async fn call<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || f(&mut pool.writer()))
            .await
            .expect("sqlite blocking task panicked")
    }

    /// Runs `f` against a free **reader** on a blocking thread, so it does not wait
    /// on a sync that is committing.
    ///
    /// Readers are `query_only`: routing a write here fails rather than silently
    /// falling back to the writer's lock.
    async fn read<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || f(&pool.reader()))
            .await
            .expect("sqlite blocking task panicked")
    }

    /// Runs `f` on a blocking thread **without** holding the connection lock — for
    /// filesystem blob I/O (a multi-megabyte read/write) that must not serialize the
    /// whole store behind the SQLite mutex the way [`Self::call`] does.
    async fn block<F, R>(f: F) -> R
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .expect("blob blocking task panicked")
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
        // Search is per-account, so every scope shares one account; the body-FTS
        // source filters on it (IMAP keys can collide across accounts).
        let account = scopes
            .first()
            .map(|scope| scope.account().as_str().to_owned())
            .unwrap_or_default();
        let query = query.clone();
        let ranked = self
            .read(move |conn| search_ops::search_mail(conn, &account, &scope_keys, &query, limit))
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
            .read(move |conn| search_ops::search_calendar(conn, &scope_keys, &query, limit))
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
    /// host reconciles a single domain without re-fetching the whole account. For mail
    /// this is the fallback for a non-QRESYNC server (a QRESYNC delta already picks up
    /// flag/move/expunge changes incrementally — `imap-smtp.md`) or a forced full
    /// re-snapshot.
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

    /// Compacts the database, reclaiming the free pages that deletions leave behind —
    /// e.g. the out-of-window messages a re-snapshot tombstones after a sync-depth
    /// reduction, or after a [`reset_sync`](Self::reset_sync) and its follow-up sync drop
    /// everything past the window. SQLite holds a file at its high-water mark and reuses
    /// freed pages rather than shrinking, so without this the on-disk size never falls as
    /// mail ages out. Runs `VACUUM` then a `TRUNCATE` checkpoint, so the main file is
    /// rewritten compact and the WAL truncated **then** — in WAL mode `VACUUM` alone defers
    /// the on-disk shrink to the next checkpoint. The content-addressed blob area is
    /// separate and untouched.
    ///
    /// It rewrites the whole database, so it needs transient free disk space about the size
    /// of the database and briefly holds the store's single connection. Call it off the hot
    /// path, once the deletions are committed (a host runs it after a reset's re-sync has
    /// settled), not on every sync.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn vacuum(&self) -> Result<()> {
        self.call(|conn| {
            // execute_batch tolerates the rows the checkpoint pragma echoes back; VACUUM
            // runs in autocommit (the bare connection holds no transaction), as it requires.
            conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(backend)
        })
        .await
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
        self.read(move |conn| scope_ops::load_state(conn, &key))
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
        T: SyncObject + Serialize + Send + Sync,
    {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        let update = OwnedUpdate::from_update(batch.update)?;
        let derived = batch.derived.clone();
        let reconcile = batch.reconcile.to_vec();
        let observations = batch.recipient_observations.to_vec();
        let contact_scope = lease.scope().object_kind() == Some(ObjectKind::ContactCard);
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
                &observations,
                contact_scope,
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

    async fn set_expansion_window(
        &self,
        lease: &SyncLease,
        window: &ExpansionWindow,
    ) -> Result<()> {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        let window = window.clone();
        self.call(move |conn| window_ops::set_expansion_window(conn, &key, token, &window))
            .await
    }

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        let key = scope_key(lease.scope());
        let token = lease.token().get();
        self.call(move |conn| scope_ops::release(conn, &key, token))
            .await
    }

    async fn abandon_sync_leases(&self) -> Result<usize> {
        self.call(scope_ops::abandon_leases).await
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

#[cfg(test)]
mod tests;
