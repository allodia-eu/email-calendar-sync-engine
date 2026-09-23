//! A write the process was cut off in the middle of, recovered at the next start-up.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use engine_api::{Engine, PendingOpKind, PendingOpState};

use super::*;

/// Counts every submission, and never answers the first: the server has the message and the
/// process ends before it hears back. Every later submission is delivered, so a second
/// delivery would show in the count.
struct CutOffProvider {
    inner: FakeProvider,
    submits: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for CutOffProvider {
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
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.submits.fetch_add(1, Ordering::SeqCst) == 0 {
            std::future::pending::<()>().await;
        }
        let key = ProviderKey::new("sent-1").unwrap();
        Ok(SubmissionReceipt::filed(key, draft.message_id.clone()))
    }
}

impl CalendarWrites for CutOffProvider {}

/// The failure recovery exists for. Before it, the op stayed `InFlight`, claimable again once
/// its lease lapsed, and pressing Send on the same message delivered it a second time.
#[tokio::test]
async fn a_send_cut_off_in_flight_is_never_delivered_twice() {
    let engine = Arc::new(Engine::open_in_memory().unwrap());
    let provider = Arc::new(CutOffProvider {
        inner: FakeProvider::new(),
        submits: AtomicUsize::new(0),
    });
    let draft = draft("gen-cut-off@test.local", "Cut off");

    let sending = tokio::spawn({
        let (engine, provider, draft) = (Arc::clone(&engine), Arc::clone(&provider), draft.clone());
        async move { engine.submit_mail(&*provider, &account(), &draft).await }
    });
    while provider.submits.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // The process ends mid-send.
    sending.abort();
    let _ = sending.await;

    // The next start-up.
    assert_eq!(engine.recover_interrupted_ops().await.unwrap(), 1);
    let queued = engine.outbox(&account()).await.unwrap();
    let send = queued
        .iter()
        .find(|row| row.kind == Some(PendingOpKind::MailSubmit))
        .expect("the send stays listed, so a host can ask about it");
    assert_eq!(send.state, PendingOpState::NeedsConfirmation);

    // The user sends the same message again. It is the same op, and it is not run again.
    assert!(
        engine
            .submit_mail(&*provider, &account(), &draft)
            .await
            .is_err()
    );
    assert_eq!(
        provider.submits.load(Ordering::SeqCst),
        1,
        "the message reached the server once"
    );
}
