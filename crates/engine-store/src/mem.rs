//! An in-memory reference [`Store`].
//!
//! This is the executable specification of the concurrency contract: it enforces
//! fencing tokens, atomic per-scope apply, snapshot tombstoning, derived-row
//! commit/tombstone, and the outbox state machine. The reusable [`contract`]
//! suite runs against it, and every real backend (`store-sqlite`, a future
//! `store-postgres`) must satisfy the same suite. It is also a useful test double
//! for `engine-sync` before a persistent store exists.
//!
//! Liveness is tracked by lease *expiry*; the fencing *token* is the actual
//! serialization mechanism (an older token is rejected even before its lease
//! expires once a newer claim bumps the generation).
//!
//! [`contract`]: crate::contract

use core::fmt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::sync::{SyncScope, SyncState, SyncUpdate};
use engine_core::time::UtcDateTime;
use engine_core::write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey};
use serde::Serialize;
use serde_json::Value;

use crate::apply::{
    ApplyBatch, DerivedWrite, FtsField, OccurrenceRow, StorableObject, SyncApplied,
};
use crate::error::{Result, StoreError};
use crate::lease::{Clock, FenceToken, LeaseRequest, OpLease, SyncClaim, SyncLease};
use crate::outbox::{LeasedPendingOp, PendingOpState};
use crate::store::{Store, StoreRead};

/// Returns `true` if a lease is held and has not expired at `now`.
fn is_live(expiry: Option<UtcDateTime>, now: UtcDateTime) -> bool {
    expiry.is_some_and(|e| e > now)
}

/// Per-scope state: the fencing generation, lease expiry, cursor, objects, and
/// derived rows.
struct ScopeCell {
    token: FenceToken,
    lease_expiry: Option<UtcDateTime>,
    state: Option<SyncState>,
    objects: HashMap<ProviderKey, Value>,
    fts: HashMap<ProviderKey, Vec<FtsField>>,
    occurrences: HashMap<ProviderKey, Vec<OccurrenceRow>>,
}

impl ScopeCell {
    fn new() -> Self {
        Self {
            token: FenceToken::initial(),
            lease_expiry: None,
            state: None,
            objects: HashMap::new(),
            fts: HashMap::new(),
            occurrences: HashMap::new(),
        }
    }

    /// Removes an object and any derived rows keyed by it. Returns whether the
    /// object existed.
    fn tombstone(&mut self, key: &ProviderKey) -> bool {
        let existed = self.objects.remove(key).is_some();
        self.fts.remove(key);
        self.occurrences.remove(key);
        existed
    }

    /// Serializes and upserts an object's normalized payload, keyed by its
    /// provider key.
    fn upsert_object<T: StorableObject + Serialize>(&mut self, obj: &T) -> Result<()> {
        let value = serde_json::to_value(obj).map_err(|e| StoreError::Backend(e.to_string()))?;
        self.objects.insert(obj.provider_key().clone(), value);
        Ok(())
    }

    /// Applies precomputed derived rows (shared by apply and maintenance).
    fn apply_derived(&mut self, derived: &DerivedWrite) {
        for row in &derived.fts {
            self.fts.insert(row.key.clone(), row.fields.clone());
        }
        for occ in &derived.occurrences {
            self.occurrences
                .entry(occ.event.clone())
                .or_default()
                .push(occ.clone());
        }
        for key in &derived.removed {
            self.fts.remove(key);
            self.occurrences.remove(key);
        }
    }
}

/// Per-op outbox state.
struct OpCell {
    account: AccountId,
    op: PendingOp,
    state: PendingOpState,
    token: FenceToken,
    lease_expiry: Option<UtcDateTime>,
}

/// The whole store state, behind one mutex (a reference impl, not a throughput
/// target).
struct Inner {
    scopes: HashMap<SyncScope, ScopeCell>,
    ops: BTreeMap<PendingOpId, OpCell>,
    idempotency: HashMap<(AccountId, IdempotencyKey), PendingOpId>,
    next_op: u64,
}

/// An in-memory [`Store`] + [`StoreRead`], parameterized by an injected [`Clock`]
/// for lease-expiry control.
pub struct MemStore<C> {
    clock: C,
    inner: Mutex<Inner>,
}

impl<C> fmt::Debug for MemStore<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemStore").finish_non_exhaustive()
    }
}

impl<C: Clock> MemStore<C> {
    /// Creates an empty store driven by `clock`.
    #[must_use]
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            inner: Mutex::new(Inner {
                scopes: HashMap::new(),
                ops: BTreeMap::new(),
                idempotency: HashMap::new(),
                next_op: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("store mutex poisoned")
    }
}

/// Computes a lease expiry from the current instant and a request's TTL.
fn expiry_after(now: UtcDateTime, req: &LeaseRequest) -> Result<UtcDateTime> {
    now.checked_add(req.ttl)
        .ok_or_else(|| StoreError::Backend("lease ttl overflow".to_owned()))
}

#[async_trait]
impl<C: Clock> Store for MemStore<C> {
    async fn load_sync_state(
        &self,
        _account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>> {
        Ok(self.lock().scopes.get(scope).and_then(|c| c.state.clone()))
    }

    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim> {
        let now = self.clock.now();
        let expiry = expiry_after(now, &req)?;
        let mut inner = self.lock();
        let cell = inner
            .scopes
            .entry(scope.clone())
            .or_insert_with(ScopeCell::new);
        if is_live(cell.lease_expiry, now) {
            return Err(StoreError::ScopeHeld);
        }
        cell.token = cell.token.bump();
        cell.lease_expiry = Some(expiry);
        let lease = SyncLease::new(account, scope.clone(), cell.token, req.owner, expiry);
        Ok(SyncClaim::new(lease, cell.state.clone()))
    }

    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: StorableObject + Serialize + Send + Sync,
    {
        let mut inner = self.lock();
        let Inner { scopes, ops, .. } = &mut *inner;
        let cell = scopes
            .get_mut(lease.scope())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != cell.token {
            return Err(StoreError::StaleLease);
        }

        let mut applied = SyncApplied::default();
        match batch.update {
            SyncUpdate::Delta { changed, removed } => {
                for obj in changed {
                    cell.upsert_object(obj)?;
                    applied.upserted += 1;
                }
                for key in removed {
                    if cell.tombstone(key) {
                        applied.tombstoned += 1;
                    }
                }
            }
            SyncUpdate::Snapshot { objects, present } => {
                for obj in objects {
                    cell.upsert_object(obj)?;
                    applied.upserted += 1;
                }
                let absent: Vec<ProviderKey> = cell
                    .objects
                    .keys()
                    .filter(|k| !present.contains(*k))
                    .cloned()
                    .collect();
                for key in absent {
                    cell.tombstone(&key);
                    applied.tombstoned += 1;
                }
            }
        }

        cell.apply_derived(batch.derived);

        for rec in batch.reconcile {
            if let Some(op) = ops.get_mut(&rec.op)
                && op.state == rec.expected
            {
                op.state = PendingOpState::Succeeded;
                op.lease_expiry = None;
                applied.reconciled += 1;
            }
        }

        cell.state = Some(batch.next_state.clone());
        Ok(applied)
    }

    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()> {
        let mut inner = self.lock();
        let cell = inner
            .scopes
            .get_mut(lease.scope())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != cell.token {
            return Err(StoreError::StaleLease);
        }
        cell.apply_derived(derived);
        Ok(())
    }

    // Takes the lease by value to consume it (the trait contract: a released
    // lease must not be reused); its fields are read by reference internally.
    #[allow(clippy::needless_pass_by_value)]
    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        let mut inner = self.lock();
        if let Some(cell) = inner.scopes.get_mut(lease.scope())
            && cell.token == lease.token()
        {
            cell.lease_expiry = None;
        }
        Ok(())
    }

    async fn enqueue_pending_op(&self, account: AccountId, op: PendingOp) -> Result<PendingOpId> {
        let mut inner = self.lock();
        let idem = (account.clone(), op.idempotency_key.clone());
        if let Some(id) = inner.idempotency.get(&idem) {
            return Ok(*id);
        }
        let id = PendingOpId::new(inner.next_op);
        inner.next_op += 1;
        inner.ops.insert(
            id,
            OpCell {
                account,
                op,
                state: PendingOpState::Pending,
                token: FenceToken::initial(),
                lease_expiry: None,
            },
        );
        inner.idempotency.insert(idem, id);
        Ok(id)
    }

    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        let now = self.clock.now();
        let expiry = expiry_after(now, &req)?;
        let LeaseRequest { owner, ttl: _ } = req;
        let mut inner = self.lock();
        let ops = &mut inner.ops;

        // Resources held by a live in-flight op cannot be re-leased this round.
        let busy: HashSet<ResourceKey> = ops
            .values()
            .filter(|o| {
                o.account == account
                    && o.state == PendingOpState::InFlight
                    && is_live(o.lease_expiry, now)
            })
            .map(|o| o.op.resource_key.clone())
            .collect();

        let mut result = Vec::new();
        let mut newly_leased: HashSet<ResourceKey> = HashSet::new();
        let ids: Vec<PendingOpId> = ops.keys().copied().collect();
        for id in ids {
            if result.len() >= limit {
                break;
            }
            // Decide with an immutable borrow, then mutate.
            let resource = {
                let Some(o) = ops.get(&id) else { continue };
                if o.account != account {
                    continue;
                }
                let claimable = matches!(o.state, PendingOpState::Pending)
                    || (matches!(o.state, PendingOpState::InFlight)
                        && !is_live(o.lease_expiry, now));
                if !claimable {
                    continue;
                }
                let deps_ok =
                    o.op.depends_on
                        .iter()
                        .all(|d| ops.get(d).is_some_and(|dep| dep.state.is_success()));
                if !deps_ok {
                    continue;
                }
                o.op.resource_key.clone()
            };
            if busy.contains(&resource) || !newly_leased.insert(resource) {
                continue;
            }
            let o = ops.get_mut(&id).expect("op present");
            o.token = o.token.bump();
            o.state = PendingOpState::InFlight;
            o.lease_expiry = Some(expiry);
            let lease = OpLease::new(o.account.clone(), id, o.token, owner.clone(), expiry);
            result.push(LeasedPendingOp::new(id, o.op.clone(), lease));
        }
        Ok(result)
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        let mut inner = self.lock();
        let op = inner
            .ops
            .get_mut(&lease.op())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != op.token {
            return Err(StoreError::StaleLease);
        }
        op.lease_expiry = None;
        match outcome {
            PendingOutcome::Succeeded { .. } => op.state = PendingOpState::Succeeded,
            PendingOutcome::Failed { .. } => op.state = PendingOpState::Failed,
            PendingOutcome::NeedsConfirmation { .. } => {
                op.state = PendingOpState::NeedsConfirmation;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl<C: Clock> StoreRead for MemStore<C> {
    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>> {
        let inner = self.lock();
        let mut keys: Vec<ProviderKey> = inner
            .scopes
            .get(scope)
            .map(|c| c.objects.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        Ok(keys)
    }

    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>> {
        let inner = self.lock();
        Ok(inner
            .scopes
            .get(scope)
            .and_then(|c| c.objects.get(key).cloned()))
    }

    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>> {
        Ok(self.lock().ops.get(&id).map(|o| o.state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::FtsRow;
    use crate::lease::{ManualClock, WorkerId};

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn expiry_after_advances_then_overflows_at_end_of_time() {
        let req = LeaseRequest::new(WorkerId::new("w"), core::time::Duration::from_secs(30));
        let early: UtcDateTime = "2026-01-01T00:00:00Z".parse().unwrap();
        assert!(expiry_after(early, &req).is_ok());

        // Past the end of representable time, expiry overflows to a backend error.
        let end_of_time: UtcDateTime = "9999-12-31T23:59:59Z".parse().unwrap();
        assert_eq!(
            expiry_after(end_of_time, &req),
            Err(StoreError::Backend("lease ttl overflow".to_owned()))
        );
    }

    #[test]
    fn apply_derived_upserts_then_removes_fts_and_occurrences() {
        let mut cell = ScopeCell::new();
        let mut derived = DerivedWrite::empty();
        derived.fts.push(FtsRow::new(
            key("e1"),
            vec![FtsField::new("summary", "standup")],
        ));
        derived.occurrences.push(OccurrenceRow {
            event: key("e1"),
            start: "2026-03-01T09:00:00Z".parse().unwrap(),
            end: "2026-03-01T09:15:00Z".parse().unwrap(),
            recurrence_id: None,
        });
        cell.apply_derived(&derived);
        assert!(cell.fts.contains_key(&key("e1")));
        assert_eq!(cell.occurrences.get(&key("e1")).map(Vec::len), Some(1));

        // A removal clears both the FTS and occurrence rows for the key.
        let mut removal = DerivedWrite::empty();
        removal.removed.push(key("e1"));
        cell.apply_derived(&removal);
        assert!(!cell.fts.contains_key(&key("e1")));
        assert!(!cell.occurrences.contains_key(&key("e1")));
    }

    #[test]
    fn mem_store_debug_is_redacted() {
        let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap());
        let store = MemStore::new(clock);
        assert!(format!("{store:?}").contains("MemStore"));
    }
}
