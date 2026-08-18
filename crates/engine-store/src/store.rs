//! The async `Store` trait and a minimal read surface.
//!
//! `store-and-sync.md` is authoritative for the concurrency model. `Store` is the
//! writer/lease/outbox half: one effective writer per scope and per in-flight op,
//! enforced by a store-issued fencing token re-checked inside the write
//! transaction. [`StoreRead`] is the small inspection surface the contract suite
//! (and, later, query execution) needs; the full search read path is a separate
//! sub-step.
//!
//! The trait is generic over the object type via [`Keyed`], so the store
//! stays mechanical and type-erased at the row level and the contract suite can
//! run on any object. It is consumed as `S: Store` (not `dyn Store`), since the
//! store sits behind `engine-api`.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, ProviderKey, ThreadId},
    mail::Keyword,
    search_index::MailRow,
    sync::{SyncObject, SyncScope, SyncState},
    time::{ExpansionWindow, Horizon},
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    apply::{ApplyBatch, DerivedWrite, OccurrenceRow, SyncApplied},
    error::Result,
    lease::{LeaseRequest, OpLease, SyncClaim, SyncLease},
    outbox::{LeasedPendingOp, PendingOpState},
};

/// The store writer, lease, and outbox contract.
///
/// Every durable state transition is lease-gated and atomic. The store performs
/// no normalization, text extraction, or recurrence expansion; pure `engine-core`
/// code precomputes the [`DerivedWrite`] carried in [`ApplyBatch`].
#[async_trait]
pub trait Store: Send + Sync {
    /// Reads a scope's current cursor without taking a lease. For diagnostics and
    /// UI only — never plan a write from this; use [`Store::claim_sync_scope`].
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` if the store cannot be read.
    async fn load_sync_state(
        &self,
        account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>>;

    /// Atomically acquires the scope lease and returns the current
    /// [`SyncState`], so the planner sees a consistent `(lease, state)` pair with
    /// no load-then-claim race. Each claim bumps the scope's fencing generation,
    /// staling any older lease.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::ScopeHeld` if a live (unexpired) lease already exists
    /// for the scope, or `StoreError::Backend` on a backend failure.
    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim>;

    /// Commits exactly one transaction for one scope, gated by the lease token:
    /// normalized objects (delta or snapshot), precomputed derived rows,
    /// pending-op reconciliations, and the next cursor — all or nothing.
    /// Replaying an identical batch under the same live lease is idempotent.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if `lease`'s token is no longer current
    /// for the scope, or `StoreError::Backend` on a backend failure.
    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: SyncObject + Serialize + Send + Sync;

    /// Writes only derived rows (FTS/occurrences) under the **same** scope lease
    /// as sync, so maintenance and sync of one scope cannot race. Used for
    /// horizon advance, timezone-data changes, and on-demand body indexing.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if `lease`'s token is no longer current,
    /// or `StoreError::Backend` on a backend failure.
    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()>;

    /// Records the window a scope's occurrence rows are now materialized over, under the
    /// **same** scope lease that wrote them.
    ///
    /// The store owns this fact because only it knows what the rows actually span
    /// (`ExpansionWindow`). A sync or a post-write reconcile re-expands a *changed* event
    /// over the window read back from here — never over a horizon its caller passed — so
    /// re-deriving one event cannot silently drop the occurrences the host already
    /// expanded. Only [`expand_calendar_horizon`](https://docs.rs/engine-sync), the call
    /// that re-expands *every* event, moves the window.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if `lease`'s token is no longer current, or
    /// `StoreError::Backend` on a backend failure.
    async fn set_expansion_window(&self, lease: &SyncLease, window: &ExpansionWindow)
    -> Result<()>;

    /// Releases a scope lease before its TTL so a finished worker does not block
    /// the next sync for the full lease window. Consumes the lease: it must not be
    /// used after release.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()>;

    /// Abandons every held sync lease after a host has established that any prior
    /// workers for this store are gone, preserving cursors and objects while
    /// bumping fencing tokens so abandoned workers cannot commit later.
    ///
    /// Intended for process-startup recovery after abrupt termination. Do not use
    /// this as an in-process contention mechanism; `StoreError::ScopeHeld` still
    /// means a live worker should finish or the caller should retry.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn abandon_sync_leases(&self) -> Result<usize>;

    /// Durably enqueues a pending op for `account`, idempotent by the op's
    /// idempotency key: re-enqueuing the same key returns the existing
    /// [`PendingOpId`] and creates no duplicate.
    ///
    /// (`store-and-sync.md` sketches this without `account`; it is required here
    /// because [`PendingOp`] carries no account and the outbox is account-scoped.)
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn enqueue_pending_op(&self, account: AccountId, op: PendingOp) -> Result<PendingOpId>;

    /// Claims up to `limit` runnable ops for `account`, each leased individually
    /// with its own fencing token. Excludes any op whose `depends_on` are not all
    /// in terminal success, and any op whose `resource_key` collides with an
    /// already-leased op.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>>;

    /// Records the outcome of a claimed op, gated by its [`OpLease`] token.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if the op was re-claimed (its token is
    /// superseded), or `StoreError::Backend` on a backend failure.
    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()>;
}

/// Counts of the derived index rows an object holds, one field per derived kind.
///
/// Returned by [`StoreRead::index_row_counts`] for contract verification and
/// diagnostics — the searchable query path is the per-store executor, not this
/// surface. `fts`, `message`, and `event_index` are 0 or 1 (one row per
/// object); the junction counts are unbounded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexRowCounts {
    /// Full-text documents (0 or 1).
    pub fts: usize,
    /// Materialized occurrences.
    pub occurrences: usize,
    /// Stored mail rows (0 or 1).
    pub message: usize,
    /// Mail address-junction rows.
    pub addresses: usize,
    /// Membership rows.
    pub memberships: usize,
    /// Event scalar-index rows (0 or 1).
    pub event_index: usize,
    /// Event participant-junction rows.
    pub participants: usize,
}

impl IndexRowCounts {
    /// Returns `true` if the object has no derived rows of any kind.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fts == 0
            && self.occurrences == 0
            && self.message == 0
            && self.addresses == 0
            && self.memberships == 0
            && self.event_index == 0
            && self.participants == 0
    }
}

/// One row of a mail list read: the stored [`MailRow`], the account it belongs to, and the
/// collections it is filed in.
///
/// The account is carried per row because one read spans several of them — an "all inboxes" view
/// is one query, not a loop with a merge in the caller. The mailboxes are carried because a host
/// decides what is in the *viewed* folder, and a message's placement is a separate axis from its
/// identity (JMAP objects hold several memberships; two IMAP copies are distinct objects with one
/// each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailListRow {
    /// The account the message belongs to.
    pub account: AccountId,
    /// The mailboxes/labels the message is filed in.
    pub mailboxes: Vec<MailboxId>,
    /// Every keyword on the message — the system ones the row's `flags` also carries as a
    /// bitfield, plus any user keyword or provider label.
    ///
    /// Carried because this read is the message's **whole mutable state**, not only the part a
    /// list paints: the stored payload deliberately holds none of it, so anything rebuilding a
    /// `Message` from storage gets the complete set here rather than from a second read that
    /// could be forgotten.
    pub keywords: Vec<Keyword>,
    /// The stored row.
    pub mail: MailRow,
}

impl MailListRow {
    /// Projects a normalized message into the list row the store would have written for it.
    ///
    /// A host that watches a sync stream sees whole [`Message`](engine_core::mail::Message)s go by
    /// and has to place them in a list it is already holding. Doing that means projecting them the
    /// way the store does — so it happens here, through the same
    /// [`project_message`](engine_core::search_index::project_message), rather than a second time
    /// in each host with its own idea of what a row's sender or preview is.
    #[must_use]
    pub fn project(account: &AccountId, message: &engine_core::mail::Message) -> Self {
        Self {
            account: account.clone(),
            mailboxes: message.mailboxes.iter().cloned().collect(),
            keywords: message.keywords.iter().cloned().collect(),
            mail: engine_core::search_index::project_message(message).row,
        }
    }
}

/// Where a store's schema stands — what the data is at, what this build expects, and whether
/// opening it moved.
///
/// A support answer, deliberately in the neutral layer rather than in one backend: "which schema
/// is this user's store on, and did this launch upgrade it" is the same question whether the rows
/// live in SQLite, in Postgres, or nowhere at all. A backend with no persistent schema reports
/// `version == expected` and never migrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaStatus {
    /// The schema version the stored data is at now.
    pub version: u32,
    /// The version this build of the engine expects.
    ///
    /// Equal to [`version`](SchemaStatus::version) on a store this build can use. A store left
    /// *ahead* of it — written by a newer build — is refused at open rather than reported here,
    /// because reading it could misinterpret a shape this build does not know.
    pub expected: u32,
    /// The version the store was at when this process opened it, if opening upgraded it.
    ///
    /// `None` when nothing moved: an already-current store, or a freshly created one. A host logs
    /// this once at startup, which is what turns "it broke after the update" into a version pair.
    pub migrated_from: Option<u32>,
}

impl SchemaStatus {
    /// A store that is at the version this build expects and did not migrate on open.
    #[must_use]
    pub fn current(version: u32) -> Self {
        Self {
            version,
            expected: version,
            migrated_from: None,
        }
    }

    /// Whether opening this store upgraded it.
    #[must_use]
    pub fn migrated(&self) -> bool {
        self.migrated_from.is_some()
    }
}

/// Which mail a [`StoreRead::list_mail`] call selects, within the accounts it is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailSelector<'a> {
    /// Everything, newest first — the windowed list a host renders.
    Newest,
    /// Every message on any of these threads, regardless of date, so a windowed list can expand a
    /// conversation into its full history. An empty slice selects nothing.
    Threads(&'a [ThreadId]),
    /// The messages named by these provider keys. Keys not found (moved, tombstoned) are simply
    /// absent; an empty slice selects nothing.
    Keys(&'a [ProviderKey]),
}

/// A minimal lease-free read/inspection surface.
///
/// Enough for the contract suite to verify stored state and for early
/// diagnostics; the structured/full-text query path is a separate sub-step.
#[async_trait]
pub trait StoreRead: Send + Sync {
    /// Where this store's schema stands: the version the data is at, the version this build
    /// expects, and what it was upgraded from if opening moved it.
    ///
    /// Lease-free and cheap — a host calls it at startup to log the store's version, and again
    /// when assembling a diagnostic report.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn schema_status(&self) -> Result<SchemaStatus>;

    /// Every sync scope the store currently knows for `account` (every scope it
    /// has claimed), in ascending [`SyncScope`] order. A per-account search
    /// enumerates these instead of hard-coding which scopes a provider uses, then
    /// routes each by
    /// [`SyncScope::search_domain`](engine_core::sync::SyncScope::search_domain).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>>;

    /// The window a scope's occurrence rows are materialized over, or `None` if nothing has
    /// expanded them yet.
    ///
    /// A lease-free read: the sync and reconcile paths resolve the window they must
    /// re-expand a changed event over *before* claiming the scope
    /// ([`Store::set_expansion_window`]).
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn expansion_window(&self, scope: &SyncScope) -> Result<Option<ExpansionWindow>>;

    /// The provider keys of live (non-tombstoned) objects in a scope.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>>;

    /// The stored normalized payload for an object, or `None` if absent or
    /// tombstoned.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>>;

    /// Every live (non-tombstoned) object in a scope as `(provider key, normalized
    /// payload)` pairs, in ascending key order. A batch read for building per-account
    /// views, so a host need not make an N+1 [`object_payload`](StoreRead::object_payload)
    /// call per key.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>>;

    /// The mail rows `select` names across `accounts`, newest first and capped at `limit`
    /// (`usize::MAX` for no cap).
    ///
    /// **This is the read a mailbox list is built from, and no other.** It returns the projected
    /// row — sender, subject, date, flags, preview — so a list costs the rows it shows, not the
    /// mail it is drawn from: a backend answers the first page from an ordered index rather than
    /// by ranking every message in the account and then opening the survivors' payloads. Reading
    /// a body or an attachment list still goes to the normalized object, on demand, for the one
    /// message being opened.
    ///
    /// Several accounts in one call is the point, not a convenience: a unified inbox is a
    /// predicate over one table, so the ordering across accounts is the backend's and not a merge
    /// the caller re-derives. An empty `accounts` selects nothing.
    ///
    /// Whether the account holds a message that is **in the message-id graph but carries no
    /// thread** — the one shape the incremental assignment cannot repair by itself.
    ///
    /// A thread is decided when a message is applied, so this is empty in steady state. It is not
    /// empty right after the migration that introduced the graph: that step backfills the graph
    /// rows from the stored payloads but assigns no thread ids, so any message the *old*
    /// whole-account pass had not yet grouped stays ungrouped — and a later arrival cannot adopt
    /// it, because the component lookup reaches a stored message only through the thread id its
    /// row already carries. Its replies would open conversations of their own, quietly, forever.
    ///
    /// Deliberately a question about the damage rather than a flag saying repair is due: a flag
    /// has to be set by whoever knew, and cleared by whoever fixed it, and is wrong if either
    /// forgets. This is true exactly when there is something to fix, so it also catches a rebuild
    /// that failed halfway and a store damaged some way nobody predicted. `engine-sync` asks it
    /// once per mail sync and repairs when the answer is yes — no host is asked to remember
    /// anything.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn has_ungrouped_graphed_mail(&self, account: &AccountId) -> Result<bool>;

    /// Rows sort newest first with undated messages last — they enter a window only if dated ones
    /// leave room — and the order is **total**: ties break on the row's own identity, so two
    /// reads of an unchanged store return the same sequence and a host reconciling by row id sees
    /// no movement.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn list_mail(
        &self,
        accounts: &[AccountId],
        select: MailSelector<'_>,
        limit: usize,
    ) -> Result<Vec<MailListRow>>;

    /// The materialized occurrences in a scope that overlap `window`, ascending by
    /// `(start, end, event)`.
    ///
    /// This is the range read a calendar grid pages over: recurrence lives in the
    /// occurrence rows, not the master event (`store-and-sync.md`), so a host that
    /// reads the events alone sees a weekly meeting once, at the series start. The
    /// window is half-open at **both** ends ([`Horizon::overlaps`]), so an event
    /// ending exactly when a week opens belongs to the previous page only, and a
    /// multi-day event that merely *covers* the window is still returned — it has to
    /// render on every day it spans.
    ///
    /// Order is **specified**, unlike a mail list read's tie-breaking,
    /// because a host lays these rows out geometrically: two hosts given the same window
    /// must place an overlapping event in the same column, and an unstable order would
    /// silently make them disagree. Empty for a non-calendar scope (only events expand)
    /// and for a scope the store has never seen. Occurrences are cleared when their
    /// event is tombstoned, so the rows are exactly the live events' — no liveness
    /// join is needed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn scope_occurrences(
        &self,
        scope: &SyncScope,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>>;

    /// The current lifecycle state of a pending op, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>>;

    /// The counts of derived index rows currently stored for an object, across
    /// every derived kind. Zero for an absent or fully-tombstoned object.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts>;
}
