//! A write whose lease is recovered under it mid-pass: another process's start-up recovery,
//! or a lease that lapsed while the pass was suspended.

use std::sync::Arc;

use engine_provider::HandOver;

use super::{store_and_clock, target};
use crate::tests::*;

/// A provider whose edits recover every op in flight before they answer, as a second process
/// starting up beside this pass would.
struct Overtaken {
    inner: FakeMail,
    store: Arc<SqliteStore<ManualClock>>,
}

impl engine_provider::MailboxWrites for Overtaken {}
impl CalendarWrites for Overtaken {}

#[async_trait::async_trait]
impl Provider for Overtaken {
    fn connection_info(&self) -> ConnectionInfo {
        self.inner.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.email_scope(account)
    }

    async fn sync_mailboxes(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        self.inner.sync_mailboxes(account, cursor).await
    }

    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        window: SyncWindow,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        self.inner
            .stream_email(account, cursor, window, fetch_batch, chunk_size)
    }

    async fn submit_email(
        &self,
        account: &AccountId,
        draft: &Draft,
        hand_over: &HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        self.inner.submit_email(account, draft, hand_over).await
    }

    async fn edit_mail(
        &self,
        account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        self.store.recover_interrupted_ops().await.unwrap();
        self.inner.edit_mail(account, edit).await
    }
}

/// An edit whose lease another process recovered while it ran cannot record its outcome, and
/// that is the other worker's business, not a reason to stop: the pass defers it and drains
/// the rest of the queue.
#[tokio::test]
async fn an_edit_overtaken_mid_pass_does_not_stop_the_queue() {
    let (store, _clock) = store_and_clock();
    let store = Arc::new(store);
    let acct = account();
    let archive = MailEdit::move_to(target(), MailboxId::try_from("Archive").unwrap());
    store
        .enqueue_pending_op(
            acct.clone(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:archive").unwrap(),
                PendingOpKind::MailEdit,
                ResourceKey::new(format!("mail:{}", target().as_str())).unwrap(),
                serde_json::to_value(&archive).unwrap(),
            ),
        )
        .await
        .unwrap();
    let report = MessageReport::new(
        target(),
        ReportVerdict::Junk,
        MailboxId::try_from("Junk").unwrap(),
    );
    store
        .enqueue_pending_op(
            acct.clone(),
            PendingOp::new(
                IdempotencyKey::new("report:u42").unwrap(),
                PendingOpKind::MailReport,
                ResourceKey::new("report:u42").unwrap(),
                serde_json::to_value(&report).unwrap(),
            ),
        )
        .await
        .unwrap();
    let provider = Overtaken {
        inner: FakeMail::new(vec![], vec![]),
        store: Arc::clone(&store),
    };

    let drained = drain_outbox(
        &provider,
        store.as_ref(),
        &acct,
        worker(),
        Duration::from_mins(1),
    )
    .await
    .expect("an overtaken edit is not the pass's failure");
    assert_eq!(drained.deferred, 1, "{drained:?}");
    assert_eq!(drained.attempted.len(), 1, "{drained:?}");
    assert_eq!(drained.attempted[0].kind, PendingOpKind::MailReport);
}
