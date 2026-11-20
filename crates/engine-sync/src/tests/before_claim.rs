//! A store that lets another pass run to completion just before a given worker claims an event
//! scope: the moment between a pass reading what the scope holds and claiming it, which no
//! timing in a test can hit reliably.
//!
//! Every method delegates to the store it wraps. The first claim of an event scope by `worker`
//! first awaits the hook, which runs against that same store.

use core::{future::Future, pin::Pin};

use async_trait::async_trait;
use engine_core::{
    ids::{MailboxId, ProviderKey},
    sync::{ObjectKind, SyncObject, SyncScope, SyncState},
    time::{ExpansionWindow, Horizon, UtcDateTime},
    version::CollectionTag,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    ApplyBatch, Confirmation, DerivedWrite, IndexRowCounts, LeaseRequest, LeasedPendingOp,
    MailListRow, MailSelector, OccurrenceRow, OpLease, OpRejection, PendingOpClaim, PendingOpRow,
    SchemaStatus, StoreError, SyncApplied, SyncClaim, SyncLease,
};
use serde::Serialize;
use serde_json::Value;

use super::*;

type Result<T> = core::result::Result<T, StoreError>;

/// What runs before the claim.
pub(super) type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

/// `inner`, except that `worker`'s first claim of an event scope waits for `hook` first.
pub(super) struct BeforeClaim<S> {
    pub(super) inner: Arc<S>,
    pub(super) worker: WorkerId,
    pub(super) hook: Mutex<Option<Hook>>,
}

#[async_trait]
impl<S: Store + StoreRead> Store for BeforeClaim<S> {
    async fn load_sync_state(
        &self,
        account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>> {
        self.inner.load_sync_state(account, scope).await
    }

    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim> {
        if req.owner == self.worker && scope.object_kind() == Some(ObjectKind::Event) {
            let hook = self.hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook.await;
            }
        }
        self.inner.claim_sync_scope(account, scope, req).await
    }

    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: SyncObject + Serialize + Send + Sync,
    {
        self.inner.apply_sync_update(lease, batch).await
    }

    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()> {
        self.inner.apply_maintenance(lease, derived).await
    }

    async fn set_expansion_window(
        &self,
        lease: &SyncLease,
        window: &ExpansionWindow,
    ) -> Result<()> {
        self.inner.set_expansion_window(lease, window).await
    }

    async fn renew_sync_scope_lease(&self, lease: &SyncLease, ttl: Duration) -> Result<()> {
        self.inner.renew_sync_scope_lease(lease, ttl).await
    }

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        self.inner.release_sync_scope(lease).await
    }

    async fn forget_scope(&self, lease: SyncLease) -> Result<()> {
        self.inner.forget_scope(lease).await
    }

    async fn abandon_sync_leases(&self) -> Result<usize> {
        self.inner.abandon_sync_leases().await
    }

    async fn recover_interrupted_ops(&self) -> Result<usize> {
        self.inner.recover_interrupted_ops().await
    }

    async fn record_hand_over(&self, lease: &OpLease) -> Result<()> {
        self.inner.record_hand_over(lease).await
    }

    async fn renew_pending_op_lease(&self, lease: &OpLease, ttl: Duration) -> Result<()> {
        self.inner.renew_pending_op_lease(lease, ttl).await
    }

    async fn enqueue_pending_op(&self, account: AccountId, op: PendingOp) -> Result<PendingOpId> {
        self.inner.enqueue_pending_op(account, op).await
    }

    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        self.inner.claim_pending_ops(account, req, limit).await
    }

    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim> {
        self.inner.claim_pending_op(account, op, req).await
    }

    async fn mark_pending_op(&self, lease: &OpLease, outcome: PendingOutcome) -> Result<()> {
        self.inner.mark_pending_op(lease, outcome).await
    }

    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.inner.cancel_pending_op(account, op).await
    }

    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.inner.retry_pending_op_now(account, op).await
    }

    async fn confirm_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        confirmation: Confirmation,
    ) -> Result<Option<OpRejection>> {
        self.inner
            .confirm_pending_op(account, op, confirmation)
            .await
    }
}

#[async_trait]
impl<S: StoreRead> StoreRead for BeforeClaim<S> {
    async fn schema_status(&self) -> Result<SchemaStatus> {
        self.inner.schema_status().await
    }

    async fn account_scopes(&self, account: AccountId) -> Result<Vec<SyncScope>> {
        self.inner.account_scopes(account).await
    }

    async fn expansion_window(&self, scope: &SyncScope) -> Result<Option<ExpansionWindow>> {
        self.inner.expansion_window(scope).await
    }

    async fn collection_tag(&self, scope: &SyncScope) -> Result<Option<CollectionTag>> {
        self.inner.collection_tag(scope).await
    }

    async fn fetch_window(&self, scope: &SyncScope) -> Result<Option<Horizon>> {
        self.inner.fetch_window(scope).await
    }

    async fn object_keys(&self, scope: &SyncScope) -> Result<Vec<ProviderKey>> {
        self.inner.object_keys(scope).await
    }

    async fn object_payload(&self, scope: &SyncScope, key: &ProviderKey) -> Result<Option<Value>> {
        self.inner.object_payload(scope, key).await
    }

    async fn scope_objects(&self, scope: &SyncScope) -> Result<Vec<(ProviderKey, Value)>> {
        self.inner.scope_objects(scope).await
    }

    async fn has_ungrouped_graphed_mail(&self, account: &AccountId) -> Result<bool> {
        self.inner.has_ungrouped_graphed_mail(account).await
    }

    async fn list_mail(
        &self,
        accounts: &[AccountId],
        select: MailSelector<'_>,
        limit: usize,
    ) -> Result<Vec<MailListRow>> {
        self.inner.list_mail(accounts, select, limit).await
    }

    async fn oldest_in_mailbox(
        &self,
        account: &AccountId,
        mailbox: &MailboxId,
    ) -> Result<Option<UtcDateTime>> {
        self.inner.oldest_in_mailbox(account, mailbox).await
    }

    async fn scope_occurrences(
        &self,
        scope: &SyncScope,
        window: Horizon,
    ) -> Result<Vec<OccurrenceRow>> {
        self.inner.scope_occurrences(scope, window).await
    }

    async fn pending_op_state(&self, id: PendingOpId) -> Result<Option<PendingOpState>> {
        self.inner.pending_op_state(id).await
    }

    async fn list_pending_ops(&self, account: AccountId) -> Result<Vec<PendingOpRow>> {
        self.inner.list_pending_ops(account).await
    }

    async fn index_row_counts(
        &self,
        scope: &SyncScope,
        key: &ProviderKey,
    ) -> Result<IndexRowCounts> {
        self.inner.index_row_counts(scope, key).await
    }
}
