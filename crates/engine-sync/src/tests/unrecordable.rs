//! A store that cannot record an op's outcome, and what each write tells its caller then.
//!
//! Every method delegates to a real store except `mark_pending_op`, which fails as a full
//! disk would. That is the moment after the provider answered: whatever the caller is told
//! must still be what the provider said, not "the store failed".

use async_trait::async_trait;
use engine_core::{
    sync::{SyncObject, SyncScope, SyncState},
    time::ExpansionWindow,
    write::{PendingOp, PendingOpId, PendingOutcome},
};
use engine_store::{
    ApplyBatch, Confirmation, DerivedWrite, LeaseRequest, LeasedPendingOp, OpLease, OpRejection,
    PendingOpClaim, StoreError, SyncApplied, SyncClaim, SyncLease,
};
use serde::Serialize;

use super::*;
use crate::SyncError;

type Result<T> = core::result::Result<T, StoreError>;

/// `inner`, except that no outcome can be recorded.
pub(super) struct Unrecordable<S>(pub(super) S);

#[async_trait]
impl<S: Store> Store for Unrecordable<S> {
    async fn load_sync_state(
        &self,
        account: AccountId,
        scope: &SyncScope,
    ) -> Result<Option<SyncState>> {
        self.0.load_sync_state(account, scope).await
    }

    async fn claim_sync_scope(
        &self,
        account: AccountId,
        scope: &SyncScope,
        req: LeaseRequest,
    ) -> Result<SyncClaim> {
        self.0.claim_sync_scope(account, scope, req).await
    }

    async fn apply_sync_update<T>(
        &self,
        lease: &SyncLease,
        batch: ApplyBatch<'_, T>,
    ) -> Result<SyncApplied>
    where
        T: SyncObject + Serialize + Send + Sync,
    {
        self.0.apply_sync_update(lease, batch).await
    }

    async fn apply_maintenance(&self, lease: &SyncLease, derived: &DerivedWrite) -> Result<()> {
        self.0.apply_maintenance(lease, derived).await
    }

    async fn set_expansion_window(
        &self,
        lease: &SyncLease,
        window: &ExpansionWindow,
    ) -> Result<()> {
        self.0.set_expansion_window(lease, window).await
    }

    async fn renew_sync_scope_lease(&self, lease: &SyncLease, ttl: Duration) -> Result<()> {
        self.0.renew_sync_scope_lease(lease, ttl).await
    }

    async fn release_sync_scope(&self, lease: SyncLease) -> Result<()> {
        self.0.release_sync_scope(lease).await
    }

    async fn forget_scope(&self, lease: SyncLease) -> Result<()> {
        self.0.forget_scope(lease).await
    }

    async fn abandon_sync_leases(&self) -> Result<usize> {
        self.0.abandon_sync_leases().await
    }

    async fn recover_interrupted_ops(&self) -> Result<usize> {
        self.0.recover_interrupted_ops().await
    }

    async fn record_hand_over(&self, lease: &OpLease) -> Result<()> {
        self.0.record_hand_over(lease).await
    }

    async fn renew_pending_op_lease(&self, lease: &OpLease, ttl: Duration) -> Result<()> {
        self.0.renew_pending_op_lease(lease, ttl).await
    }

    async fn enqueue_pending_op(&self, account: AccountId, op: PendingOp) -> Result<PendingOpId> {
        self.0.enqueue_pending_op(account, op).await
    }

    async fn claim_pending_ops(
        &self,
        account: AccountId,
        req: LeaseRequest,
        limit: usize,
    ) -> Result<Vec<LeasedPendingOp>> {
        self.0.claim_pending_ops(account, req, limit).await
    }

    async fn claim_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        req: LeaseRequest,
    ) -> Result<PendingOpClaim> {
        self.0.claim_pending_op(account, op, req).await
    }

    async fn mark_pending_op(&self, _lease: &OpLease, _outcome: PendingOutcome) -> Result<()> {
        Err(StoreError::Backend("disk full".to_owned()))
    }

    async fn cancel_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.0.cancel_pending_op(account, op).await
    }

    async fn retry_pending_op_now(
        &self,
        account: AccountId,
        op: PendingOpId,
    ) -> Result<Option<OpRejection>> {
        self.0.retry_pending_op_now(account, op).await
    }

    async fn confirm_pending_op(
        &self,
        account: AccountId,
        op: PendingOpId,
        confirmation: Confirmation,
    ) -> Result<Option<OpRejection>> {
        self.0.confirm_pending_op(account, op, confirmation).await
    }
}

fn store() -> Unrecordable<SqliteStore<ManualClock>> {
    Unrecordable(SqliteStore::open_in_memory(clock()).unwrap())
}

async fn send(provider: &FakeMail) -> SyncError {
    submit_mail(
        provider,
        &store(),
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("unrecorded@test.local"),
    )
    .await
    .expect_err("the outcome could not be recorded")
}

/// A delivered message, or one that may have been, is never reported as a store failure:
/// a host reading that would tell the user it was not sent.
#[tokio::test]
async fn a_send_whose_outcome_cannot_be_recorded_still_says_it_may_have_gone() {
    let delivered = send(&FakeMail::new(vec![], vec![])).await;
    assert!(
        matches!(delivered, SyncError::Unrecorded(_)),
        "{delivered:?}"
    );
    let unknown = send(&FakeMail::new(vec![], vec![]).failing(Fault::AmbiguousSubmit)).await;
    assert!(matches!(unknown, SyncError::Unrecorded(_)), "{unknown:?}");
    // Refused: nothing was sent, so a store error is the honest answer.
    let refused = send(&FakeMail::new(vec![], vec![]).failing(Fault::PermanentSubmit)).await;
    assert!(matches!(refused, SyncError::Store(_)), "{refused:?}");
}

/// A calendar write the server refused, whose refusal cannot be recorded, is still the
/// refusal: nothing was applied, and the caller needs its class to recover.
#[tokio::test]
async fn a_refused_write_whose_refusal_cannot_be_recorded_keeps_the_refusal() {
    let provider = FakeMail::new(vec![], vec![]).failing(Fault::WriteGuard);
    let draft = EventDraft::new(
        CalendarId::new(ProviderKey::new("/cal/default/").unwrap()),
        Uid::new("evt-u@test.local").unwrap(),
        "Planning",
        CalendarDateTime::utc("2026-08-01T09:00:00".parse::<LocalDateTime>().unwrap()),
        CalendarDateTime::utc("2026-08-01T10:00:00".parse::<LocalDateTime>().unwrap()),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    );
    let err = create_calendar_event(
        &provider,
        &store(),
        &account(),
        worker(),
        Duration::from_mins(1),
        "refused-create",
        &draft,
    )
    .await
    .expect_err("refused");
    match err {
        SyncError::Provider(err) => assert_eq!(err.class(), FailureClass::Conflict),
        other => panic!("expected the provider's refusal, got {other:?}"),
    }
}
