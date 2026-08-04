//! Providers that misbehave on purpose, wrapping the stateful `CalendarServer`.
//!
//! Both exist to drive the two ways a **post-write reconcile** can fail while the write
//! itself already landed: the event scope is held by a concurrent sync (`BlockingSync`), or
//! the delta call itself errors (`UnreadableEvents`). Neither may be reported as a failed
//! write — the server has the change, and a host that re-issued it would write twice.
//!
//! A sibling file so `calendar_writes.rs` stays under the line limit.

use super::*;

/// A provider whose event fetch parks until it is released, so a test can hold the event
/// scope's lease while another call tries to reconcile.
pub(super) struct BlockingSync {
    pub(super) inner: CalendarServer,
    pub(super) started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(super) release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Provider for BlockingSync {
    fn connection_info(&self) -> ConnectionInfo {
        self.inner.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.email_scope(account)
    }

    async fn sync_events(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            // The lease is held across this await — exactly the window a real concurrent
            // sync leaves open.
            let _ = release.await;
        }
        self.inner.sync_events(account, cursor).await
    }
}

/// A provider whose writes land but whose event fetch is broken, so the post-write
/// reconcile fails on its own rather than on a held lease.
pub(super) struct UnreadableEvents(pub(super) CalendarServer);

#[async_trait::async_trait]
impl Provider for UnreadableEvents {
    fn connection_info(&self) -> ConnectionInfo {
        self.0.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.0.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.0.email_scope(account)
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Err(ProviderError::retryable("the event fetch is down"))
    }

    async fn patch_event(
        &self,
        account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        self.0.patch_event(account, base, edit).await
    }
}
