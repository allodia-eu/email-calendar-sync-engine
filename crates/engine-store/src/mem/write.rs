//! The [`Store`](crate::Store) write path for `MemStore`: claim, apply
//! (delta/snapshot), maintenance, release, and the outbox op state machine.

use std::collections::HashSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{SyncScope, SyncState, SyncUpdate},
    write::{PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use serde::Serialize;

use super::{Inner, MemStore, OpCell, ScopeCell, expiry_after, is_live};
use crate::{
    apply::{ApplyBatch, DerivedWrite, StorableObject, SyncApplied},
    error::{Result, StoreError},
    lease::{Clock, FenceToken, LeaseRequest, OpLease, SyncClaim, SyncLease},
    outbox::{LeasedPendingOp, PendingOpState},
    store::Store,
};

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

        // A streaming page (`next_state == None`) leaves the cursor unchanged.
        if let Some(next_state) = batch.next_state {
            cell.state = Some(next_state.clone());
        }
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

    async fn abandon_sync_leases(&self) -> Result<usize> {
        let mut abandoned = 0;
        let mut inner = self.lock();
        for cell in inner.scopes.values_mut() {
            if cell.lease_expiry.is_some() {
                cell.token = cell.token.bump();
                cell.lease_expiry = None;
                abandoned += 1;
            }
        }
        Ok(abandoned)
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
