//! The [`Store`](crate::Store) write path for `MemStore`: claim, apply
//! (delta/snapshot), maintenance, release, and the outbox op state machine.

use std::collections::HashSet;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{ObjectKind, SyncObject, SyncScope, SyncState, SyncUpdate},
    time::{ExpansionWindow, UtcDateTime},
    write::{PendingOp, PendingOpId, PendingOutcome, ResourceKey},
};
use serde::Serialize;

use super::{
    Inner, MemStore, ObservationCell, OpCell, ScopeCell, expiry_after, is_live, threading,
};
use crate::{
    apply::{ApplyBatch, DerivedWrite, SyncApplied},
    error::{Result, StoreError},
    lease::{Clock, FenceToken, LeaseRequest, OpLease, SyncClaim, SyncLease},
    outbox::{
        ClaimRejection, LeasedPendingOp, MAX_ATTEMPTS, OpRejection, PendingOpClaim, PendingOpState,
        retry_delay,
    },
    store::Store,
};

/// Whether a parked retry's backoff has elapsed. An op with no `next_attempt_at`
/// has never failed and is due immediately.
fn is_due(next_attempt_at: Option<UtcDateTime>, now: UtcDateTime) -> bool {
    next_attempt_at.is_none_or(|due| due <= now)
}

/// Whether an op may be leased now: fresh and due, or one whose lease died under it.
/// A kind-less row (enqueued before the store recorded one) is never runnable, since
/// nothing says which request type its payload is.
fn is_runnable(cell: &OpCell, now: UtcDateTime) -> bool {
    match cell.state {
        PendingOpState::Pending => is_due(cell.next_attempt_at, now),
        PendingOpState::InFlight => !is_live(cell.lease_expiry, now),
        PendingOpState::Succeeded
        | PendingOpState::Failed
        | PendingOpState::Cancelled
        | PendingOpState::NeedsConfirmation => false,
    }
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
        T: SyncObject + Serialize + Send + Sync,
    {
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
            if let Some(op) = ops.get_mut(&rec.op)
                && op.state == rec.expected
            {
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
                attempts: 0,
                next_attempt_at: None,
                failure_class: None,
                detail: None,
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
                if !is_runnable(o, now) {
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

    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim> {
        let now = self.clock.now();
        let expiry = expiry_after(now, &req)?;
        let LeaseRequest { owner, ttl: _ } = req;
        let mut inner = self.lock();
        let ops = &mut inner.ops;

        let Some(cell) = ops.get(&op).filter(|o| o.account == account) else {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
        };
        // The batch claim's own predicate: a fresh op, or one whose lease died under it.
        match cell.state {
            PendingOpState::Pending if is_due(cell.next_attempt_at, now) => {}
            // Parked on a backoff after a retryable failure: it will run, just not yet.
            PendingOpState::Pending => {
                return Ok(PendingOpClaim::Refused(ClaimRejection::Backoff));
            }
            PendingOpState::InFlight if !is_live(cell.lease_expiry, now) => {}
            PendingOpState::InFlight => {
                return Ok(PendingOpClaim::Refused(ClaimRejection::Busy));
            }
            PendingOpState::Succeeded
            | PendingOpState::Failed
            | PendingOpState::Cancelled
            | PendingOpState::NeedsConfirmation => {
                return Ok(PendingOpClaim::Refused(ClaimRejection::Settled));
            }
        }
        let resource = cell.op.resource_key.clone();
        let depends_on = cell.op.depends_on.clone();
        if !depends_on.iter().all(|d| {
            ops.get(d)
                .is_some_and(|dep| dep.account == account && dep.state.is_success())
        }) {
            return Ok(PendingOpClaim::Refused(ClaimRejection::DependencyUnmet));
        }
        // Another op holding this one's resource under a live lease serializes against
        // it, exactly as in the batch claim.
        let held = ops.iter().any(|(id, o)| {
            *id != op
                && o.account == account
                && o.op.resource_key == resource
                && o.state == PendingOpState::InFlight
                && is_live(o.lease_expiry, now)
        });
        if held {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Busy));
        }

        let cell = ops.get_mut(&op).expect("op present");
        cell.token = cell.token.bump();
        cell.state = PendingOpState::InFlight;
        cell.lease_expiry = Some(expiry);
        let lease = OpLease::new(account, op, cell.token, owner, expiry);
        Ok(PendingOpClaim::Leased(Box::new(LeasedPendingOp::new(
            op,
            cell.op.clone(),
            lease,
        ))))
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let op = inner
            .ops
            .get_mut(&lease.op())
            .ok_or(StoreError::StaleLease)?;
        if lease.token() != op.token {
            return Err(StoreError::StaleLease);
        }
        op.lease_expiry = None;
        op.attempts = op.attempts.saturating_add(1);
        match outcome {
            PendingOutcome::Succeeded { .. } => {
                op.state = PendingOpState::Succeeded;
                op.next_attempt_at = None;
                op.failure_class = None;
                op.detail = None;
            }
            PendingOutcome::Failed { class, retry_after } => {
                op.failure_class = Some(class);
                op.detail = None;
                // A class that a plain retry cannot fix settles now; so does one that
                // has used up its attempts. Everything else parks and comes back.
                if class.is_retryable() && op.attempts < MAX_ATTEMPTS {
                    op.state = PendingOpState::Pending;
                    op.next_attempt_at = now.checked_add(retry_delay(op.attempts, retry_after));
                } else {
                    op.state = PendingOpState::Failed;
                    op.next_attempt_at = None;
                }
            }
            PendingOutcome::NeedsConfirmation { detail } => {
                op.state = PendingOpState::NeedsConfirmation;
                op.next_attempt_at = None;
                op.detail = Some(detail);
            }
        }
        Ok(())
    }

    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let Some(cell) = inner.ops.get_mut(&op).filter(|o| o.account == account) else {
            return Ok(Some(OpRejection::Unknown));
        };
        match cell.state {
            // A dead lease is nobody's side effect: the worker that held it is gone.
            PendingOpState::Pending => {}
            PendingOpState::InFlight if !is_live(cell.lease_expiry, now) => {}
            PendingOpState::InFlight => return Ok(Some(OpRejection::InFlight)),
            PendingOpState::NeedsConfirmation => {
                return Ok(Some(OpRejection::AwaitingConfirmation));
            }
            PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::Cancelled => {
                return Ok(Some(OpRejection::Settled));
            }
        }
        // Bump the token so a worker still holding the old lease cannot resolve it.
        cell.token = cell.token.bump();
        cell.state = PendingOpState::Cancelled;
        cell.lease_expiry = None;
        cell.next_attempt_at = None;
        Ok(None)
    }

    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let Some(cell) = inner.ops.get_mut(&op).filter(|o| o.account == account) else {
            return Ok(Some(OpRejection::Unknown));
        };
        match cell.state {
            // A dead lease is nobody's attempt: the worker that held it is gone.
            PendingOpState::Pending => {}
            PendingOpState::InFlight if !is_live(cell.lease_expiry, now) => {}
            PendingOpState::InFlight => return Ok(Some(OpRejection::InFlight)),
            PendingOpState::NeedsConfirmation => {
                return Ok(Some(OpRejection::AwaitingConfirmation));
            }
            PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::Cancelled => {
                return Ok(Some(OpRejection::Settled));
            }
        }
        // The attempt count stays: one more attempt now, not a fresh bound.
        cell.next_attempt_at = None;
        Ok(None)
    }
}
