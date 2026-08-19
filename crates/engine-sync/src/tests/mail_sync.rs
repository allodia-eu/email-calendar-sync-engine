//! Loop mechanics over a real store driven by fake providers: container and
//! member persistence + indexing, empty-delta resync, and `StaleLease`
//! re-claim-and-recompute. Uses the shared fakes and helpers from the parent
//! module via `use super::*`.

use futures_util::StreamExt;

use super::*;

#[tokio::test]
async fn sync_mail_persists_containers_members_and_index() {
    let provider = FakeMail::new(
        vec![
            mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("h", "Archive", None),
        ],
        vec![
            message("m1", "a", "Quarterly report"),
            message("m2", "a", "Lunch plans"),
        ],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert_eq!(
        report
            .mailboxes
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .upserted,
        2
    );
    assert_eq!(report.upserted(), 2);

    // Containers landed under the mailbox scope.
    let mailbox_scope = provider.mailbox_scope(&account());
    assert_eq!(store.object_keys(&mailbox_scope).await.unwrap().len(), 2);

    // Members landed under the email scope, with derived index rows (searchable).
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 2);
    let counts = store
        .index_row_counts(&email_scope, &key("m1"))
        .await
        .unwrap();
    assert!(counts.fts >= 1, "expected a full-text row");
    assert!(counts.message >= 1, "expected a stored mail row");
    assert!(counts.memberships >= 1, "expected a membership row");

    // The cursor advanced.
    let cursor = store
        .load_sync_state(account(), &email_scope)
        .await
        .unwrap();
    assert_eq!(cursor.as_ref().map(SyncState::as_str), Some("cursor-1"));
}

#[tokio::test]
async fn resync_with_cursor_applies_empty_delta() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;

    // Second run: a cursor now exists, so the fake returns an empty delta.
    let report = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert_eq!(report.upserted(), 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 1);
}

#[tokio::test]
async fn sync_mail_observes_sent_recipients_and_records_window_coverage() {
    let mut sent_message = message("sent-1", "sent", "Hello");
    sent_message.envelope.to = vec![EmailAddress::named("Friend", "friend@example.test")];
    sent_message.envelope.cc = vec![EmailAddress::new("cc@example.test")];
    sent_message.envelope.bcc = vec![EmailAddress::new("bcc@example.test")];
    let provider = FakeMail::new(
        vec![mailbox("sent", "Sent", Some(MailboxRole::Sent))],
        vec![sent_message],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;

    let interactions = store.recipient_interactions(None).await.unwrap();
    assert_eq!(interactions.len(), 3);
    assert!(
        interactions
            .iter()
            .any(|item| item.email.as_str() == "bcc@example.test")
    );
    let coverage = store.recipient_coverage(Some(account())).await.unwrap();
    assert_eq!(coverage.len(), 1);
    assert!(coverage[0].sent_collection_identified);
    assert_eq!(coverage[0].window, SyncWindow::full());

    // The provider replays no changed messages on the next cursor pass, and the
    // source-message key keeps the aggregate idempotent.
    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;
    assert!(
        store
            .recipient_interactions(None)
            .await
            .unwrap()
            .iter()
            .all(|item| item.sent_count == 1)
    );
}

/// Wraps a [`FakeMail`] and, on the first email fetch, expires the loop's lease
/// (advancing the shared clock) then steals + releases the scope — forcing the
/// loop's apply to fail `StaleLease` and re-claim.
struct LeaseStealer {
    inner: FakeMail,
    store: Arc<SqliteStore<ManualClock>>,
    clock: ManualClock,
    stolen: AtomicBool,
}

#[async_trait::async_trait]
impl Provider for LeaseStealer {
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
        Box::pin(async_stream::try_stream! {
            if !self.stolen.swap(true, Ordering::SeqCst) {
                // Advance past the loop's lease TTL so its lease has expired, then
                // claim + release as another worker to bump the fencing generation —
                // before the first chunk applies, forcing a mid-stream `StaleLease`.
                self.clock.advance(Duration::from_mins(2));
                let scope = self.inner.email_scope(account);
                let claim = self
                    .store
                    .claim_sync_scope(
                        account.clone(),
                        &scope,
                        LeaseRequest::new(WorkerId::new("intruder"), Duration::from_mins(1)),
                    )
                    .await
                    .unwrap();
                self.store.release_sync_scope(claim.lease).await.unwrap();
            }
            let mut inner =
                self.inner.stream_email(account, cursor, window, fetch_batch, chunk_size);
            while let Some(item) = inner.next().await {
                yield item?;
            }
        })
    }
}

#[tokio::test]
async fn stale_lease_triggers_reclaim_and_recompute() {
    let clock = clock();
    let store = Arc::new(SqliteStore::open_in_memory(clock.clone()).unwrap());
    let provider = LeaseStealer {
        inner: FakeMail::new(
            vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
            vec![message("m1", "a", "Hello")],
        ),
        store: Arc::clone(&store),
        clock,
        stolen: AtomicBool::new(false),
    };

    // The loop's first email apply is stale (the steal bumped the generation during
    // fetch); it re-claims with the fresh state and recomputes to success.
    let report = sync_mail(
        core::slice::from_ref(&provider),
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await;

    assert!(
        provider.stolen.load(Ordering::SeqCst),
        "the steal must have run"
    );
    assert_eq!(report.upserted(), 1);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 1);
}
