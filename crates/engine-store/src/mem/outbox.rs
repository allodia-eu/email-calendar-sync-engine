//! The outbox op state machine of `MemStore`: claims, outcomes, recovery of an attempt whose
//! worker is gone, and the host's verbs. `write.rs` delegates to these, so the trait impl
//! there stays a list of entry points.

use std::collections::{BTreeMap, HashSet};

use engine_core::{
    ids::AccountId,
    time::UtcDateTime,
    write::{PendingOpId, PendingOutcome, ResourceKey},
};

use super::{MemStore, OpCell, expiry_after, is_live};
use crate::{
    error::{Result, StoreError},
    lease::{Clock, LeaseRequest, OpLease},
    outbox::{
        ClaimRejection, Confirmation, LeasedPendingOp, OpRejection, PendingOpClaim, PendingOpState,
    },
    settle::{Recorded, interrupted_outcome, record_outcome, stays_listed},
};

type Ops = BTreeMap<PendingOpId, OpCell>;

impl OpCell {
    /// Where `outcome` would leave this op, without changing it.
    fn recorded(&self, outcome: &PendingOutcome, now: UtcDateTime) -> Result<Recorded> {
        record_outcome(Some(self.op.kind), self.attempts, outcome, now)
    }

    /// Records `outcome`, releasing the lease and counting the attempt.
    fn record(&mut self, outcome: &PendingOutcome, now: UtcDateTime) -> Result<()> {
        let recorded = self.recorded(outcome, now)?;
        self.lease_expiry = None;
        self.state = recorded.state;
        self.attempts = recorded.attempts;
        self.next_attempt_at = recorded.next_attempt_at;
        self.failure_class = recorded.failure_class;
        self.detail = recorded.detail;
        if !recorded.keeps_hand_over {
            self.handed_over = None;
        }
        Ok(())
    }

    /// This op as a read shows it: a dead attempt as its recovery will leave it, so a host
    /// never sees a send stuck "in flight" under a worker that is gone. The recovery itself
    /// is durable only on a write path.
    pub(super) fn view(&self, now: UtcDateTime) -> Result<Recorded> {
        if self.is_dead(now) {
            return self.recorded(&self.interrupted(), now);
        }
        Ok(Recorded {
            state: self.state,
            attempts: self.attempts,
            next_attempt_at: self.next_attempt_at,
            failure_class: self.failure_class,
            detail: self.detail.clone(),
            keeps_hand_over: self.handed_over.is_some(),
        })
    }

    /// What recovering this op's attempt records: the interrupted outcome for its kind.
    fn interrupted(&self) -> PendingOutcome {
        interrupted_outcome(self.op.kind, self.handed_over.is_some())
    }

    /// Whether this op's attempt is gone: `InFlight` under a lease that has lapsed.
    pub(super) fn is_dead(&self, now: UtcDateTime) -> bool {
        self.state == PendingOpState::InFlight && !is_live(self.lease_expiry, now)
    }

    /// Recovers this op's attempt and fences its worker out.
    pub(super) fn recover(&mut self, now: UtcDateTime) -> Result<()> {
        let outcome = self.interrupted();
        self.record(&outcome, now)?;
        self.token = self.token.bump();
        Ok(())
    }

    /// Takes the op from whatever worker held it and makes it due at once, clearing any
    /// hand-over so a late answer from that worker cannot settle the next attempt.
    fn rearm(&mut self) {
        self.token = self.token.bump();
        self.state = PendingOpState::Pending;
        self.lease_expiry = None;
        self.next_attempt_at = None;
        self.handed_over = None;
        self.detail = None;
    }
}

/// Recovers every dead attempt of `account`.
fn recover_dead(ops: &mut Ops, account: &AccountId, now: UtcDateTime) -> Result<()> {
    for cell in ops.values_mut() {
        if cell.account == *account && cell.is_dead(now) {
            cell.recover(now)?;
        }
    }
    Ok(())
}

/// Whether a lease on an op other than `except` holds `resource`. Called once dead attempts
/// are recovered, so every `InFlight` op is a live one.
fn resource_held(
    ops: &Ops,
    account: &AccountId,
    resource: &ResourceKey,
    except: PendingOpId,
) -> bool {
    ops.iter().any(|(id, o)| {
        *id != except
            && o.account == *account
            && o.op.resource_key == *resource
            && o.state == PendingOpState::InFlight
    })
}

/// Whether every op in `cell`'s `depends_on` has succeeded.
fn dependencies_met(ops: &Ops, account: &AccountId, cell: &OpCell) -> bool {
    cell.op.depends_on.iter().all(|d| {
        ops.get(d)
            .is_some_and(|dep| dep.account == *account && dep.state.is_success())
    })
}

/// Leases `cell` until `expiry`.
fn lease(
    cell: &mut OpCell,
    id: PendingOpId,
    req: &LeaseRequest,
    expiry: UtcDateTime,
) -> LeasedPendingOp {
    cell.token = cell.token.bump();
    cell.state = PendingOpState::InFlight;
    cell.lease_expiry = Some(expiry);
    let lease = OpLease::new(
        cell.account.clone(),
        id,
        cell.token,
        req.owner.clone(),
        expiry,
    );
    LeasedPendingOp::new(id, cell.op.clone(), lease)
}

/// Why a host verb that withdraws or hurries cannot act on `cell`, once any dead attempt is
/// recovered. A send that settled `Failed` is still the host's to act on.
fn host_refusal(cell: &OpCell) -> Option<OpRejection> {
    match cell.state {
        PendingOpState::Pending => None,
        PendingOpState::InFlight => Some(OpRejection::InFlight),
        PendingOpState::NeedsConfirmation => Some(OpRejection::AwaitingConfirmation),
        PendingOpState::Failed if stays_listed(Some(cell.op.kind), cell.state) => None,
        PendingOpState::Succeeded | PendingOpState::Failed | PendingOpState::Cancelled => {
            Some(OpRejection::Settled)
        }
    }
}

impl<C: Clock> MemStore<C> {
    /// [`Store::recover_interrupted_ops`](crate::Store::recover_interrupted_ops).
    pub(super) fn recover_interrupted(&self) -> Result<usize> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let mut recovered = 0;
        for op in inner.ops.values_mut() {
            if op.state == PendingOpState::InFlight {
                op.recover(now)?;
                recovered += 1;
            }
        }
        inner.settle_keyword_edits(now);
        Ok(recovered)
    }

    /// [`Store::claim_pending_ops`](crate::Store::claim_pending_ops).
    pub(super) fn claim_ops(
        &self,
        account: &AccountId,
        req: &LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        let now = self.clock.now();
        let expiry = expiry_after(now, req)?;
        let mut inner = self.lock();
        recover_dead(&mut inner.ops, account, now)?;
        inner.settle_keyword_edits(now);
        let ops = &mut inner.ops;

        // Resources held by an op in flight cannot be leased this round.
        let mut taken: HashSet<ResourceKey> = ops
            .values()
            .filter(|o| o.account == *account && o.state == PendingOpState::InFlight)
            .map(|o| o.op.resource_key.clone())
            .collect();
        let mut result = Vec::new();
        let ids: Vec<PendingOpId> = ops.keys().copied().collect();
        for id in ids {
            if result.len() >= limit {
                break;
            }
            let Some(o) = ops.get(&id) else { continue };
            let runnable = o.account == *account
                && o.state == PendingOpState::Pending
                && o.next_attempt_at.is_none_or(|due| due <= now)
                && dependencies_met(ops, account, o);
            if !runnable || !taken.insert(o.op.resource_key.clone()) {
                continue;
            }
            let cell = ops.get_mut(&id).expect("op present");
            result.push(lease(cell, id, req, expiry));
        }
        Ok(result)
    }

    /// [`Store::claim_pending_op`](crate::Store::claim_pending_op).
    pub(super) fn claim_one(
        &self,
        account: &AccountId,
        id: PendingOpId,
        req: &LeaseRequest,
    ) -> Result<PendingOpClaim> {
        let now = self.clock.now();
        let expiry = expiry_after(now, req)?;
        let mut inner = self.lock();
        recover_dead(&mut inner.ops, account, now)?;
        inner.settle_keyword_edits(now);
        let ops = &mut inner.ops;
        let Some(cell) = ops.get(&id).filter(|o| o.account == *account) else {
            return Ok(PendingOpClaim::Refused(ClaimRejection::Unknown));
        };
        let refusal = match cell.state {
            PendingOpState::Pending if cell.next_attempt_at.is_none_or(|due| due <= now) => None,
            // Parked on a backoff after a retryable failure: it will run, just not yet.
            PendingOpState::Pending => Some(ClaimRejection::Backoff),
            PendingOpState::InFlight => Some(ClaimRejection::Busy),
            PendingOpState::Succeeded
            | PendingOpState::Failed
            | PendingOpState::Cancelled
            | PendingOpState::NeedsConfirmation => Some(ClaimRejection::Settled),
        }
        .or_else(|| {
            (!dependencies_met(ops, account, cell)).then_some(ClaimRejection::DependencyUnmet)
        })
        .or_else(|| {
            resource_held(ops, account, &cell.op.resource_key, id).then_some(ClaimRejection::Busy)
        });
        if let Some(refusal) = refusal {
            return Ok(PendingOpClaim::Refused(refusal));
        }
        let cell = ops.get_mut(&id).expect("op present");
        Ok(PendingOpClaim::Leased(Box::new(lease(
            cell, id, req, expiry,
        ))))
    }

    /// [`Store::mark_pending_op`](crate::Store::mark_pending_op).
    pub(super) fn mark(&self, lease: &OpLease, outcome: &PendingOutcome) -> Result<()> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let cell = inner
            .ops
            .get_mut(&lease.op())
            .ok_or(StoreError::StaleLease)?;
        let current = lease.token() == cell.token;
        // The attempt that handed a send over, after a recovery parked it for confirmation.
        let handed_over = cell.state == PendingOpState::NeedsConfirmation
            && cell.handed_over == Some(lease.token());
        if !current && !handed_over {
            return Err(StoreError::StaleLease);
        }
        cell.record(outcome, now)?;
        inner.settle_keyword_edits(now);
        Ok(())
    }

    /// [`Store::record_hand_over`](crate::Store::record_hand_over).
    pub(super) fn hand_over(&self, lease: &OpLease) -> Result<()> {
        let mut inner = self.lock();
        let cell = inner
            .ops
            .get_mut(&lease.op())
            .filter(|o| o.token == lease.token() && o.state == PendingOpState::InFlight)
            .ok_or(StoreError::StaleLease)?;
        cell.handed_over = Some(lease.token());
        Ok(())
    }

    /// [`Store::renew_pending_op_lease`](crate::Store::renew_pending_op_lease).
    pub(super) fn renew(&self, lease: &OpLease, ttl: core::time::Duration) -> Result<()> {
        let now = self.clock.now();
        let expiry = now
            .checked_add(ttl)
            .ok_or_else(|| StoreError::Backend("lease ttl overflow".to_owned()))?;
        let mut inner = self.lock();
        let cell = inner
            .ops
            .get_mut(&lease.op())
            .filter(|o| o.token == lease.token() && o.state == PendingOpState::InFlight)
            .ok_or(StoreError::StaleLease)?;
        cell.lease_expiry = Some(expiry);
        Ok(())
    }

    /// Finds `id` in `account`'s outbox with any dead attempt recovered, then lets `act`
    /// decide; `None` from `act` means it took effect.
    fn host_verb(
        &self,
        account: &AccountId,
        id: PendingOpId,
        act: impl FnOnce(&mut OpCell) -> Option<OpRejection>,
    ) -> Result<Option<OpRejection>> {
        let now = self.clock.now();
        let mut inner = self.lock();
        let Some(cell) = inner.ops.get_mut(&id).filter(|o| o.account == *account) else {
            return Ok(Some(OpRejection::Unknown));
        };
        if cell.is_dead(now) {
            cell.recover(now)?;
        }
        let refused = act(cell);
        inner.settle_keyword_edits(now);
        Ok(refused)
    }

    /// [`Store::cancel_pending_op`](crate::Store::cancel_pending_op).
    pub(super) fn cancel(
        &self,
        account: &AccountId,
        id: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.host_verb(account, id, |cell| {
            if let Some(refusal) = host_refusal(cell) {
                return Some(refusal);
            }
            // Bump the token so a worker still holding the old lease cannot resolve it.
            cell.token = cell.token.bump();
            cell.state = PendingOpState::Cancelled;
            cell.lease_expiry = None;
            cell.next_attempt_at = None;
            None
        })
    }

    /// [`Store::retry_pending_op_now`](crate::Store::retry_pending_op_now).
    pub(super) fn retry_now(
        &self,
        account: &AccountId,
        id: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.host_verb(account, id, |cell| {
            if let Some(refusal) = host_refusal(cell) {
                return Some(refusal);
            }
            if cell.state == PendingOpState::Failed {
                // A send the outbox gave up on, sent again.
                cell.rearm();
            }
            // The attempt count stays: one more attempt now, not a fresh bound.
            cell.next_attempt_at = None;
            None
        })
    }

    /// [`Store::confirm_pending_op`](crate::Store::confirm_pending_op).
    pub(super) fn confirm(
        &self,
        account: &AccountId,
        id: PendingOpId,
        confirmation: Confirmation,
    ) -> Result<Option<OpRejection>> {
        self.host_verb(account, id, |cell| {
            match cell.state {
                PendingOpState::NeedsConfirmation => {}
                PendingOpState::InFlight => return Some(OpRejection::InFlight),
                _ => return Some(OpRejection::NotAwaitingConfirmation),
            }
            match confirmation {
                Confirmation::Delivered => {
                    cell.token = cell.token.bump();
                    cell.state = PendingOpState::Succeeded;
                    cell.handed_over = None;
                }
                Confirmation::NotDelivered => cell.rearm(),
            }
            None
        })
    }
}
