//! The async `Store` trait and a minimal read surface.
//!
//! `store-and-sync.md` is authoritative for the concurrency model. `Store` is the
//! writer/lease/outbox half: one effective writer per scope and per in-flight op,
//! enforced by a store-issued fencing token re-checked inside the write
//! transaction. [`StoreRead`] is the small inspection surface the contract suite
//! (and, later, query execution) needs; the full search read path is a separate
//! sub-step.
//!
//! The trait is generic over the object type via [`StorableObject`], so the store
//! stays mechanical and type-erased at the row level and the contract suite can
//! run on any object. It is consumed as `S: Store` (not `dyn Store`), since the
//! store sits behind `engine-api`.

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncState};
use engine_core::write::{PendingOp, PendingOpId, PendingOutcome};
use serde::Serialize;
use serde_json::Value;

use crate::apply::{ApplyBatch, DerivedWrite, StorableObject, SyncApplied};
use crate::error::Result;
use crate::lease::{LeaseRequest, OpLease, SyncClaim, SyncLease};
use crate::outbox::{LeasedPendingOp, PendingOpState};

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
        T: StorableObject + Serialize + Send + Sync;

    /// Writes only derived rows (FTS/occurrences) under the **same** scope lease
    /// as sync, so maintenance and sync of one scope cannot race. Used for
    /// horizon advance, timezone-data changes, and on-demand body indexing.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if `lease`'s token is no longer current,
    /// or `StoreError::Backend` on a backend failure.
    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()>;

    /// Releases a scope lease before its TTL so a finished worker does not block
    /// the next sync for the full lease window. Consumes the lease: it must not be
    /// used after release.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()>;

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

/// A minimal lease-free read/inspection surface.
///
/// Enough for the contract suite to verify stored state and for early
/// diagnostics; the structured/full-text query path is a separate sub-step.
#[async_trait]
pub trait StoreRead: Send + Sync {
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

    /// The current lifecycle state of a pending op, or `None` if unknown.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure.
    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>>;
}
