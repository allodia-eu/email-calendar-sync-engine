//! Leases, fencing tokens, and the clock seam.
//!
//! There is one serialization mechanism, not two (`store-and-sync.md`): a
//! store-issued lease carrying a monotonic fencing token. A write is admitted iff
//! its token still equals the scope's (or op's) current generation, re-checked
//! inside the write transaction. The token *is* the compare-and-swap key —
//! leasing and CAS are one mechanism here.
//!
//! These are the pure handles the store mints and the orchestrator round-trips.
//! The async `Store` trait that issues and checks them lives in `crate`.

use core::time::Duration;
use std::sync::{Arc, Mutex};

use engine_core::ids::AccountId;
use engine_core::sync::{SyncScope, SyncState};
use engine_core::time::UtcDateTime;
use engine_core::write::PendingOpId;
use serde::{Deserialize, Serialize};

/// Identifies the worker holding or requesting a lease.
///
/// Host-assigned and opaque to the engine; used for lease ownership and
/// diagnostics, never for serialization decisions (the fencing token does that).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerId(Box<str>);

impl WorkerId {
    /// Wraps a worker identity string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().into_boxed_str())
    }

    /// Returns the identity as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A monotonic fencing token.
///
/// Each scope or op *claim* bumps the stored generation; an older lease's token
/// is then stale, and `apply_sync_update` / `mark_pending_op` reject it. Apply
/// does not bump the token, so repeated applies under one live lease are allowed
/// (and idempotent); only a fresh claim supersedes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FenceToken(u64);

impl FenceToken {
    /// The generation a never-leased scope or op starts at.
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    /// Reconstructs a token from a persisted generation value.
    ///
    /// A durable store (`store-sqlite`, a future `store-postgres`) round-trips the
    /// fencing generation through storage as an integer and rebuilds the token on
    /// read, then [`bump`](Self::bump)s it on the next claim. The in-memory
    /// reference store keeps the token live and never needs this; `from_generation(0)`
    /// equals [`initial`](Self::initial).
    #[must_use]
    pub const fn from_generation(generation: u64) -> Self {
        Self(generation)
    }

    /// Returns the next, strictly-greater generation (minted by a new claim).
    #[must_use]
    pub const fn bump(self) -> Self {
        Self(self.0 + 1)
    }

    /// Returns the raw generation value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A request to acquire a scope or outbox lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// The worker that will hold the lease.
    pub owner: WorkerId,
    /// How long the lease stays valid before another worker may re-claim. An
    /// elapsed wall-clock span, not a calendar duration.
    pub ttl: Duration,
}

impl LeaseRequest {
    /// Creates a lease request.
    #[must_use]
    pub fn new(owner: WorkerId, ttl: Duration) -> Self {
        Self { owner, ttl }
    }
}

/// An opaque, store-issued lease over one `(account, scope)`.
///
/// Carries the fencing token the store re-checks inside the write transaction,
/// plus the bound identity, owner, and expiry. Minted by a store when granting a
/// claim; the orchestrator treats it as an opaque handle and plans a write only
/// from a fresh claim, never from a lease-free read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncLease {
    account: AccountId,
    scope: SyncScope,
    token: FenceToken,
    owner: WorkerId,
    expiry: UtcDateTime,
}

impl SyncLease {
    /// Mints a lease. Called by a store impl when granting `claim_sync_scope`.
    #[must_use]
    pub fn new(
        account: AccountId,
        scope: SyncScope,
        token: FenceToken,
        owner: WorkerId,
        expiry: UtcDateTime,
    ) -> Self {
        Self {
            account,
            scope,
            token,
            owner,
            expiry,
        }
    }

    /// The account this lease is bound to.
    #[must_use]
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    /// The scope whose writes this lease serializes.
    #[must_use]
    pub fn scope(&self) -> &SyncScope {
        &self.scope
    }

    /// The fencing token re-checked at apply time.
    #[must_use]
    pub fn token(&self) -> FenceToken {
        self.token
    }

    /// The worker that holds the lease.
    #[must_use]
    pub fn owner(&self) -> &WorkerId {
        &self.owner
    }

    /// When the lease expires and another worker may re-claim the scope.
    #[must_use]
    pub fn expiry(&self) -> UtcDateTime {
        self.expiry
    }
}

/// An opaque, store-issued lease over one in-flight outbox op.
///
/// The outbox is fenced exactly like the sync path: a suspended-then-resumed
/// worker must not clobber an op another worker already re-claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpLease {
    account: AccountId,
    op: PendingOpId,
    token: FenceToken,
    owner: WorkerId,
    expiry: UtcDateTime,
}

impl OpLease {
    /// Mints an op lease. Called by a store impl when claiming a pending op.
    #[must_use]
    pub fn new(
        account: AccountId,
        op: PendingOpId,
        token: FenceToken,
        owner: WorkerId,
        expiry: UtcDateTime,
    ) -> Self {
        Self {
            account,
            op,
            token,
            owner,
            expiry,
        }
    }

    /// The account this op belongs to.
    #[must_use]
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    /// The pending op this lease covers.
    #[must_use]
    pub fn op(&self) -> PendingOpId {
        self.op
    }

    /// The fencing token re-checked when the outcome is reported.
    #[must_use]
    pub fn token(&self) -> FenceToken {
        self.token
    }

    /// The worker that holds the lease.
    #[must_use]
    pub fn owner(&self) -> &WorkerId {
        &self.owner
    }

    /// When the lease expires and the op may be re-claimed.
    #[must_use]
    pub fn expiry(&self) -> UtcDateTime {
        self.expiry
    }
}

/// The result of `claim_sync_scope`: a lease paired with the scope's current
/// cursor, so the planner sees a consistent `(lease, state)` with no
/// load-then-claim race. `state` is `None` for a never-synced scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncClaim {
    /// The acquired lease.
    pub lease: SyncLease,
    /// The scope's current cursor, if it has synced before.
    pub state: Option<SyncState>,
}

impl SyncClaim {
    /// Pairs a lease with the scope's current cursor.
    #[must_use]
    pub fn new(lease: SyncLease, state: Option<SyncState>) -> Self {
        Self { lease, state }
    }
}

/// The engine's injectable time source (`north-star.md`).
///
/// Drives lease expiry today, and retry backoff and confirmation timeouts later.
/// Kept as a trait so tests use a controllable clock and hosts supply the
/// platform clock; the engine never reads wall-clock time directly.
pub trait Clock: Send + Sync {
    /// Returns the current instant.
    fn now(&self) -> UtcDateTime;
}

/// A controllable [`Clock`] for tests and ephemeral hosts.
///
/// Cloning shares the same underlying instant, so a test can build a store with
/// one handle and advance time through another. Production hosts supply a
/// real-time clock instead; the engine itself never reads wall-clock time.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<UtcDateTime>>);

impl ManualClock {
    /// Creates a clock fixed at `start`.
    #[must_use]
    pub fn new(start: UtcDateTime) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    /// Advances the clock by an elapsed span.
    ///
    /// # Panics
    ///
    /// Panics on representational overflow (a test clock never reaches it) or if
    /// the internal lock is poisoned.
    pub fn advance(&self, by: Duration) {
        let mut guard = self.0.lock().expect("manual clock poisoned");
        *guard = guard.checked_add(by).expect("manual clock overflow");
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UtcDateTime {
        *self.0.lock().expect("manual clock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountId {
        AccountId::try_from("acct-1").unwrap()
    }

    fn scope() -> SyncScope {
        SyncScope::JmapType {
            account: account(),
            data_type: engine_core::sync::JmapDataType::Email,
        }
    }

    fn instant() -> UtcDateTime {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn fence_token_is_monotonic_from_initial() {
        let t0 = FenceToken::initial();
        assert_eq!(t0.get(), 0);
        assert_eq!(t0.bump().get(), 1);
        assert!(t0 < t0.bump());
    }

    #[test]
    fn fence_token_rehydrates_from_persisted_generation() {
        // A durable store reads the generation back as an integer and rebuilds the
        // token; the zero generation is exactly `initial`.
        assert_eq!(FenceToken::from_generation(0), FenceToken::initial());
        assert_eq!(FenceToken::from_generation(5).get(), 5);
        // Rehydrate-then-claim is the next strictly-greater generation.
        assert_eq!(FenceToken::from_generation(5).bump().get(), 6);
    }

    #[test]
    fn worker_id_and_request_roundtrip() {
        let req = LeaseRequest::new(WorkerId::new("w-1"), Duration::from_secs(30));
        assert_eq!(req.owner.as_str(), "w-1");
        assert_eq!(req.ttl, Duration::from_secs(30));
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<LeaseRequest>(&json).unwrap(), req);
    }

    #[test]
    fn sync_lease_exposes_bound_identity_token_and_expiry() {
        let lease = SyncLease::new(
            account(),
            scope(),
            FenceToken::initial().bump(),
            WorkerId::new("w-1"),
            instant(),
        );
        assert_eq!(lease.account(), &account());
        assert_eq!(lease.scope(), &scope());
        assert_eq!(lease.token().get(), 1);
        assert_eq!(lease.owner().as_str(), "w-1");
        assert_eq!(lease.expiry(), instant());
        // Claim pairs the lease with an absent cursor for a never-synced scope.
        let claim = SyncClaim::new(lease.clone(), None);
        assert_eq!(claim.lease, lease);
        assert!(claim.state.is_none());
    }

    #[test]
    fn op_lease_binds_account_and_op() {
        let lease = OpLease::new(
            account(),
            PendingOpId::new(7),
            FenceToken::initial().bump(),
            WorkerId::new("w-2"),
            instant(),
        );
        assert_eq!(lease.account(), &account());
        assert_eq!(lease.op(), PendingOpId::new(7));
        assert_eq!(lease.token().get(), 1);
        assert_eq!(lease.owner().as_str(), "w-2");
        assert_eq!(lease.expiry(), instant());
    }

    #[test]
    fn manual_clock_advances_and_is_shared_across_clones() {
        let clock = ManualClock::new(instant());
        let handle = clock.clone();
        assert_eq!(clock.now(), instant());
        handle.advance(Duration::from_secs(90));
        // The advance is visible through the original handle (shared state).
        assert_eq!(clock.now().to_string(), "2026-01-01T00:01:30Z");
    }
}
