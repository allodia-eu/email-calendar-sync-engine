//! The [`Store`](crate::Store) write path for `MemStore`: claim, apply
//! (delta/snapshot), maintenance, release, and the outbox op state machine.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{ObjectKind, SyncObject, SyncScope, SyncState, SyncUpdate},
    time::ExpansionWindow,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use serde::Serialize;

use super::{
    Inner, MemStore, ObservationCell, OpCell, ScopeCell, expiry_after, is_live, threading,
};
use crate::{
    apply::{ApplyBatch, DerivedWrite, SyncApplied},
    error::{Result, StoreError},
    lease::{Clock, FenceToken, LeaseRequest, OpLease, SyncClaim, SyncLease},
    outbox::{Confirmation, LeasedPendingOp, OpRejection, PendingOpClaim, PendingOpState},
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
        T: SyncObject + Serialize + Send + Sync,
    {
        let now = self.clock.now();
        let mut inner = self.lock();
        let is_contact = lease.scope().object_kind() == Some(ObjectKind::ContactCard);
        let Inner {
            scopes,
            ops,
            contact_generation,
            observations,
            ..
        } = &mut *inner;
        let cell = scopes
            .get_mut(lease.scope())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != cell.token {
            return Err(StoreError::StaleLease);
        }

        let mut applied = SyncApplied::default();
        match batch.update {
            // `patched` is deliberately not read here: the partials were projected into
            // `batch.derived` before the call, which is how a store receives them. Nothing in a
            // patch belongs in a payload.
            SyncUpdate::Delta {
                changed, removed, ..
            } => {
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
            let Some(op) = ops.get_mut(&rec.op) else {
                continue;
            };
            // A read shows a dead attempt as its recovery will leave it, and the planner
            // decided on that view, so recover it before comparing.
            if op.is_dead(now) {
                op.recover(now)?;
            }
            if op.state == rec.expected {
                op.state = PendingOpState::Succeeded;
                op.lease_expiry = None;
                applied.reconciled += 1;
            }
        }

        for observation in batch.recipient_observations {
            let key = (
                observation.account.clone(),
                observation.source_message.clone(),
                observation.email.clone(),
            );
            observations.entry(key).or_insert_with(|| ObservationCell {
                observation: observation.clone(),
                suppressed: false,
            });
        }

        if is_contact && (applied.upserted > 0 || applied.tombstoned > 0) {
            *contact_generation = contact_generation.saturating_add(1);
        }

        // A streaming page (`next_state == None`) leaves the cursor unchanged.
        if let Some(next_state) = batch.next_state {
            cell.state = Some(next_state.clone());
        }

        // Last, and across the **account** rather than the scope: a reply in Sent and its
        // original in the Inbox are one conversation in two scopes, so the merge has to see
        // both — which means the borrow of this scope's cell has to have ended.
        if !batch.derived.messages.is_empty() {
            let scope = lease.scope().clone();
            threading::assign_threads(
                scopes,
                scope.account(),
                &scope,
                &batch.derived.messages,
                &batch.derived.msgid_refs,
            );
        }

        // The server's word on these messages' keywords is in; a change still queued goes back
        // on top of it.
        let written: Vec<ProviderKey> = batch
            .derived
            .messages
            .iter()
            .map(|row| row.key.clone())
            .chain(
                batch
                    .derived
                    .state_changes
                    .iter()
                    .map(|row| row.key.clone()),
            )
            .collect();
        let scope = lease.scope().clone();
        inner.keyword_edits_synced(scope.account(), &scope, &written, batch.observed_from);
        inner.settle_keyword_edits(now);
        Ok(applied)
    }

    async fn set_expansion_window(
        &self,
        lease: &SyncLease,
        window: &ExpansionWindow,
    ) -> Result<()> {
        let mut inner = self.lock();
        let cell = inner
            .scopes
            .get_mut(lease.scope())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != cell.token {
            return Err(StoreError::StaleLease);
        }
        cell.window = Some(window.clone());
        Ok(())
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

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        self.release_scope(&lease);
        Ok(())
    }

    async fn forget_scope(&self, lease: SyncLease) -> Result<()> {
        self.forget_scope_under(&lease)
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

    async fn recover_interrupted_ops(&self) -> Result<usize> {
        self.recover_interrupted()
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
                attempts: 0,
                next_attempt_at: None,
                failure_class: None,
                detail: None,
                handed_over: None,
                settled_at: None,
            },
        );
        inner.idempotency.insert(idem, id);
        if let Some(edit) = inner
            .ops
            .get(&id)
            .and_then(|cell| cell.op.keyword_edit.clone())
        {
            let account = inner.ops[&id].account.clone();
            inner.keyword_edit_queued(&account, &edit);
        }
        Ok(id)
    }

    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        self.claim_ops(&account, &req, limit)
    }

    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim> {
        self.claim_one(&account, op, &req)
    }

    async fn record_hand_over(&self, lease: &OpLease) -> Result<()> {
        self.hand_over(lease)
    }

    async fn renew_pending_op_lease(
        &self,
        lease: &OpLease,
        ttl: core::time::Duration,
    ) -> Result<()> {
        self.renew(lease, ttl)
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        self.mark(lease, &outcome)
    }

    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.cancel(&account, op)
    }

    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.retry_now(&account, op)
    }

    async fn confirm_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        confirmation: Confirmation,
    ) -> Result<Option<OpRejection>> {
        self.confirm(&account, op, confirmation)
    }
}
