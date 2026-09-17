//! The async `Store` trait: the writer, lease and outbox half.
//!
//! `store-and-sync.md` is authoritative for the concurrency model. One effective
//! writer per scope and per in-flight op, enforced by a store-issued fencing token
//! re-checked inside the write transaction. The lease-free inspection surface is
//! [`StoreRead`](crate::StoreRead), in `read.rs`.
//!
//! The trait is generic over the object type via [`Keyed`], so the store
//! stays mechanical and type-erased at the row level and the contract suite can
//! run on any object. It is consumed as `S: Store` (not `dyn Store`), since the
//! store sits behind `engine-api`.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    sync::{SyncObject, SyncScope, SyncState},
    time::ExpansionWindow,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use serde::Serialize;

use crate::{
    apply::{ApplyBatch, DerivedWrite, SyncApplied},
    error::Result,
    lease::{LeaseRequest, OpLease, SyncClaim, SyncLease},
    outbox::{LeasedPendingOp, OpRejection, PendingOpClaim},
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

    /// Claims the **one** op `op` names, under the same runnable rules as
    /// [`claim_pending_ops`](Store::claim_pending_ops), returning why it refused when
    /// it cannot.
    ///
    /// The batch claim is ordered and bounded, which makes it the wrong primitive for
    /// resolving a *particular* op: once an account's runnable set exceeds the batch
    /// size, no batch reaches the tail, and an inline driver's own op is always at the
    /// tail. Leasing exactly the op named also leases nothing a caller will not
    /// resolve.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure. A refusal is not an error:
    /// it is [`PendingOpClaim::Refused`].
    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim>;

    /// Records the outcome of a claimed op, gated by its [`OpLease`] token.
    ///
    /// A [`Failed`](PendingOutcome::Failed) outcome whose class
    /// [`is_retryable`](engine_core::error::FailureClass::is_retryable) does **not** settle
    /// the op: it parks back in [`Pending`](crate::PendingOpState::Pending) with its attempt
    /// count raised and a `next_attempt_at` from
    /// [`retry_delay`](crate::retry_delay), and becomes claimable again once that time
    /// passes. It settles as `Failed` once the attempts reach
    /// [`MAX_ATTEMPTS`](crate::MAX_ATTEMPTS). Every other class settles immediately:
    /// a conflict or an auth failure needs recomputation or a human, and backing off
    /// changes neither.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::StaleLease` if the op was re-claimed (its token is
    /// superseded), or `StoreError::Backend` on a backend failure.
    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()>;

    /// Withdraws a queued op so it is never attempted, settling it as
    /// [`Cancelled`](crate::PendingOpState::Cancelled).
    ///
    /// The host's half of an outbox a user can see: a message queued to the wrong address
    /// has to be stoppable, and no other call removes an op from the runnable set. Refuses
    /// rather than errors when the op cannot be withdrawn, because the caller acts on the
    /// difference (see [`OpRejection`]).
    ///
    /// The row itself stays: it is the `(account, idempotency_key)` record that makes
    /// enqueuing idempotent, and deleting one re-arms a replay of that write.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure. A refusal is not an error.
    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>>;

    /// Clears a queued op's retry backoff so the next drain pass attempts it, returning
    /// `None` when it did and the reason when it could not.
    ///
    /// The other half of an outbox a user can see: a person looking at a message that has
    /// not gone will press the button rather than wait out a delay the engine chose, and
    /// a bounded wait the user is watching is a long one. The attempt count is **not**
    /// reset: asking for one more attempt now is not asking for the bound to start again,
    /// and an op that has used up its attempts has already settled and is refused here.
    ///
    /// An op with no backoff to clear is not a refusal: it is already due, which is what
    /// the caller wanted.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Backend` on a backend failure. A refusal is not an error.
    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>>;
}
