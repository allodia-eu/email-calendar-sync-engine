//! Streaming mail-sync tests: incremental page commit + host visibility,
//! progress reporting, the delta-page path, and mid-stream `StaleLease` restart.
//! Uses the shared fakes and helpers from the parent module via `use super::*`.

use super::*;

/// A mail provider that yields email in fixed pages and, from page two on, asserts
/// the previous pages' rows are already committed — proving each page is applied
/// (and host-visible) before the next is fetched. Can optionally steal its own
/// lease just before one page to exercise mid-stream `StaleLease` recovery.
struct PagedMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    pages: Vec<Vec<Message>>,
    cursor: SyncState,
    store: Arc<SqliteStore<ManualClock>>,
    clock: ManualClock,
    steal_before: Option<usize>,
    stolen: AtomicBool,
}

impl PagedMail {
    fn new(
        mailboxes: Vec<Mailbox>,
        pages: Vec<Vec<Message>>,
        store: Arc<SqliteStore<ManualClock>>,
        clock: ManualClock,
    ) -> Self {
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes,
            pages,
            cursor: SyncState::new("cursor-1"),
            store,
            clock,
            steal_before: None,
            stolen: AtomicBool::new(false),
        }
    }

    /// Steals the loop's lease (as another worker) just before the page at `index`
    /// is applied, forcing that apply to fail `StaleLease` exactly once.
    fn stealing_before(mut self, index: usize) -> Self {
        self.steal_before = Some(index);
        self
    }

    fn total(&self) -> usize {
        self.pages.iter().map(Vec::len).sum()
    }
}

#[async_trait::async_trait]
impl Provider for PagedMail {
    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Email,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            self.cursor.clone(),
        ))
    }

    async fn sync_email_page(
        &self,
        account: &AccountId,
        _cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        _limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        // The fake's opaque token is just the next page index.
        let index: usize = page.map_or(0, |t| t.as_str().parse().unwrap());
        // Optionally steal the lease right before this page's apply (once), to
        // force a mid-stream `StaleLease` and exercise restart-from-scratch.
        if self.steal_before == Some(index) && !self.stolen.swap(true, Ordering::SeqCst) {
            self.clock.advance(Duration::from_mins(2));
            let scope = self.email_scope(account);
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
        if index > 0 {
            // Each earlier page must already be committed and host-visible before
            // this one is requested — that is what "streaming" buys the UI.
            let visible = self
                .store
                .object_keys(&self.email_scope(account))
                .await
                .unwrap()
                .len();
            let expected: usize = self.pages[..index].iter().map(Vec::len).sum();
            assert_eq!(
                visible, expected,
                "page {index} was fetched before earlier pages committed"
            );
        }
        let messages = self.pages[index].clone();
        let present: Vec<ProviderKey> = messages.iter().map(|m| m.id.key().clone()).collect();
        let next_page =
            (index + 1 < self.pages.len()).then(|| PageToken::new((index + 1).to_string()));
        Ok(SyncPage {
            kind: SyncKind::Snapshot,
            changed: messages,
            removed: Vec::new(),
            present,
            next_page,
            next_cursor: self.cursor.clone(),
            total: Some(self.total()),
        })
    }
}

#[tokio::test]
async fn streamed_email_commits_each_page_and_reports_progress() {
    let store = Arc::new(SqliteStore::open_in_memory(clock()).unwrap());
    // Five messages over three pages (2 + 2 + 1).
    let provider = PagedMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            vec![message("m1", "a", "One"), message("m2", "a", "Two")],
            vec![message("m3", "a", "Three"), message("m4", "a", "Four")],
            vec![message("m5", "a", "Five")],
        ],
        Arc::clone(&store),
        clock(),
    );

    // A closure progress sink (exercising the `Fn` blanket impl) records the run.
    let recorded: Mutex<Vec<SyncProgress>> = Mutex::new(Vec::new());
    let report = sync_mail_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        2,
        &|progress: SyncProgress| recorded.lock().unwrap().push(progress),
    )
    .await
    .unwrap();

    // Every message committed across the pages; the closing snapshot tombstoned
    // nothing (the accumulated present set covered them all).
    assert_eq!(report.email.upserted, 5);
    assert_eq!(report.email.tombstoned, 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 5);

    // Progress advanced 2 → 4 → 5, always against the known total of 5 and for the
    // email scope.
    let seq = recorded.lock().unwrap();
    assert_eq!(
        seq.iter().map(|p| p.fetched).collect::<Vec<_>>(),
        vec![2, 4, 5]
    );
    assert!(seq.iter().all(|p| p.total == Some(5)));
    assert!(seq.iter().all(|p| p.scope == email_scope));
}

#[tokio::test]
async fn mailbox_list_sync_applies_folders_without_email() {
    // The once-per-account container step: only the mailbox list is applied; the email
    // scope stays untouched, so the per-folder email streams can fan out afterwards.
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let applied = sync_mailbox_list(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(applied.upserted, 1); // the one folder
    assert_eq!(
        store
            .object_keys(&provider.mailbox_scope(&account()))
            .await
            .unwrap()
            .len(),
        1
    );
    // Email was deliberately not synced by the list-only call.
    assert!(
        store
            .object_keys(&provider.email_scope(&account()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn folder_email_stream_commits_email_without_a_mailbox_sync() {
    // The per-folder counterpart streams only email — no mailbox-list step — and
    // reports progress, so several folders can run it concurrently after one list sync.
    let store = Arc::new(SqliteStore::open_in_memory(clock()).unwrap());
    let provider = PagedMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            vec![message("m1", "a", "One"), message("m2", "a", "Two")],
            vec![message("m3", "a", "Three")],
        ],
        Arc::clone(&store),
        clock(),
    );

    let recorded: Mutex<Vec<SyncProgress>> = Mutex::new(Vec::new());
    let applied = sync_email_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        2,
        &|progress: SyncProgress| recorded.lock().unwrap().push(progress),
    )
    .await
    .unwrap();

    assert_eq!(applied.upserted, 3);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 3);
    // The mailbox scope was never touched by the email-only stream.
    assert!(
        store
            .object_keys(&provider.mailbox_scope(&account()))
            .await
            .unwrap()
            .is_empty()
    );
    // Progress reported per committed page (2 → 3) for the email scope.
    let seq = recorded.lock().unwrap();
    assert_eq!(
        seq.iter().map(|p| p.fetched).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(seq.iter().all(|p| p.scope == email_scope));
}

#[tokio::test]
async fn streamed_resync_applies_a_delta_page() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let noop = |_progress: SyncProgress| {};

    // First streamed sync lands the snapshot.
    sync_mail_streamed(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        10,
        &noop,
    )
    .await
    .unwrap();
    // Second: a cursor now exists, so the email stream is a single empty delta page.
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        10,
        &noop,
    )
    .await
    .unwrap();
    assert_eq!(report.email.upserted, 0);
    assert_eq!(
        store
            .object_keys(&provider.email_scope(&account()))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn streamed_email_survives_a_midstream_stale_lease() {
    let clock = clock();
    let store = Arc::new(SqliteStore::open_in_memory(clock.clone()).unwrap());
    // Steal the lease right before page two — after page one has already committed.
    // The loop's page-two apply fails `StaleLease`, abandons the partial stream
    // (the cursor was never advanced), re-claims, and re-streams from scratch.
    let provider = PagedMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            vec![message("m1", "a", "One"), message("m2", "a", "Two")],
            vec![message("m3", "a", "Three"), message("m4", "a", "Four")],
            vec![message("m5", "a", "Five")],
        ],
        Arc::clone(&store),
        clock,
    )
    .stealing_before(1);

    let noop = |_progress: SyncProgress| {};
    let report = sync_mail_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        2,
        &noop,
    )
    .await
    .unwrap();

    assert!(
        provider.stolen.load(Ordering::SeqCst),
        "the steal must have run"
    );
    // The held cursor made the restart safe: all five land exactly once, none
    // duplicated or tombstoned by the abandoned partial pass.
    assert_eq!(report.email.upserted, 5);
    assert_eq!(report.email.tombstoned, 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 5);
}
