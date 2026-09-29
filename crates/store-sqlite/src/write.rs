//! The [`Store`] impl for [`SqliteStore`]: the sync writer/lease path and the outbox.
//!
//! Each method is a thin delegation onto a blocking connection call; the SQL and the
//! decisions live in the `*_ops` modules. Split from `lib.rs`, which keeps the store's
//! construction and its shared helpers.

use async_trait::async_trait;
use engine_core::{
    ids::AccountId,
    sync::{ObjectKind, SyncObject, SyncScope, SyncState},
    time::ExpansionWindow,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    ApplyBatch, DerivedWrite, LeaseRequest, LeasedPendingOp, OpLease, OpRejection, PendingOpClaim,
    Result, Store, SyncApplied, SyncClaim, SyncLease,
};
use serde::Serialize;

use crate::{
    Clock, SqliteStore, convert::expiry_after, outbox_ops, scope_key, scope_ops,
    scope_ops::OwnedUpdate, window_ops,
};

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

    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim> {
        let now = self.clock.now();
        let expiry = expiry_after(now, req.ttl)?;
        let owner = req.owner;
        self.call(move |conn| outbox_ops::claim_one(conn, &account, op, &owner, now, expiry))
            .await
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        let op_id = lease.op();
        let token = lease.token().get();
        let now = self.clock.now();
        self.call(move |conn| outbox_ops::mark(conn, op_id, token, now, &outcome))
            .await
    }

    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        let now = self.clock.now();
        self.call(move |conn| outbox_ops::cancel(conn, &account, op, now))
            .await
    }

    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        let now = self.clock.now();
        self.call(move |conn| outbox_ops::retry_now(conn, &account, op, now))
            .await
    }
}
