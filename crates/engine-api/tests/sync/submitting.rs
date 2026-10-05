//! A provider whose submissions succeed or fail as a test asks, for the outbox-mediated
//! submission facade.

use engine_provider::{MailEditReceipt, ReportReceipt};

use super::*;

/// Wraps a [`FakeProvider`] and overrides `submit_email` to succeed (filing the
/// sent copy under a fixed key, echoing the draft's `Message-ID`) or fail, so the
/// outbox-mediated submission facade can be exercised. Other methods delegate.
pub(super) struct SubmittingProvider {
    pub(super) inner: FakeProvider,
    pub(super) fail: bool,
    /// Deliver, but report the sender's copy as unfiled (the two-step-transport case).
    pub(super) unfiled: bool,
}

#[async_trait::async_trait]
impl Provider for SubmittingProvider {
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
        hand_over: &engine_provider::HandOver<'_>,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.fail {
            return Err(ProviderError::retryable("smtp is offline"));
        }
        let handed_over = hand_over.commit().await?;
        let (key, id) = (
            ProviderKey::new("sent-1").unwrap(),
            draft.message_id.clone(),
        );
        if self.unfiled {
            let detail = "APPEND failed: connection reset";
            return Ok(SubmissionReceipt::unfiled(key, id, detail, &handed_over));
        }
        Ok(SubmissionReceipt::filed(key, id, &handed_over))
    }

    /// Stores a draft, answering with a key that **moves on a re-save**, which is what
    /// three of the four real adapters do. A fake that echoed the key back would let a
    /// facade test pass while the caller kept a stale key and stored a second copy.
    async fn put_draft(
        &self,
        _account: &AccountId,
        _draft: &Draft,
        replacing: Option<&ProviderKey>,
    ) -> ProviderResult<ProviderKey> {
        if self.fail {
            return Err(ProviderError::retryable("no route to host"));
        }
        let key = if replacing.is_some() {
            "draft-2"
        } else {
            "draft-1"
        };
        Ok(ProviderKey::new(key).unwrap())
    }

    async fn delete_draft(&self, _account: &AccountId, _draft: &ProviderKey) -> ProviderResult<()> {
        if self.fail {
            return Err(ProviderError::retryable("no route to host"));
        }
        Ok(())
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        if self.fail {
            return Err(ProviderError::conflict("UIDVALIDITY changed"));
        }
        Ok(MailEditReceipt::new(edit.target().clone()))
    }

    /// The reporting half, so the outbox path for a report is exercised through the same
    /// fake as an edit. `accept` is what the real adapters do with the capability *before*
    /// they touch the network, so a verdict the controls exclude must be refused here too —
    /// otherwise the test would prove the outbox records an op no provider would accept.
    async fn report_message(
        &self,
        _account: &AccountId,
        report: &MessageReport,
    ) -> ProviderResult<ReportReceipt> {
        report_controls().accept(report)?;
        if self.fail {
            return Err(ProviderError::conflict("the message moved"));
        }
        Ok(ReportReceipt::new(report.target.clone()))
    }
}

impl engine_provider::MailboxWrites for SubmittingProvider {}
impl CalendarWrites for SubmittingProvider {}
