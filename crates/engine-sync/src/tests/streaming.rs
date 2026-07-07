//! Streaming mail-sync tests: incremental chunk commit + host visibility, the
//! change events an observer sees, progress aggregation, the delta path, mid-stream
//! `StaleLease` restart, and resume-from-checkpoint after a killed cold sync. Uses
//! the shared fakes and helpers from the parent module via `use super::*`.

use engine_provider::EmailChunk;

use super::*;

/// Per-commit records a test observer collects: `(fetched, total, upserted keys)`.
type Commits = Mutex<Vec<(usize, Option<usize>, Vec<String>)>>;

/// A mail provider that streams email in fixed chunks and, from chunk two on,
/// asserts the previous chunks' rows are already committed — proving each chunk is
/// applied (and host-visible) before the next is produced. Can optionally steal its
/// own lease just before one chunk to exercise mid-stream `StaleLease` recovery.
struct ChunkedMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    chunks: Vec<Vec<Message>>,
    cursor: SyncState,
    store: Arc<SqliteStore<ManualClock>>,
    clock: ManualClock,
    steal_before: Option<usize>,
    stolen: AtomicBool,
}

impl ChunkedMail {
    fn new(
        mailboxes: Vec<Mailbox>,
        chunks: Vec<Vec<Message>>,
        store: Arc<SqliteStore<ManualClock>>,
        clock: ManualClock,
    ) -> Self {
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes,
            chunks,
            cursor: SyncState::new("cursor-1"),
            store,
            clock,
            steal_before: None,
            stolen: AtomicBool::new(false),
        }
    }

    fn stealing_before(mut self, index: usize) -> Self {
        self.steal_before = Some(index);
        self
    }

    fn total(&self) -> usize {
        self.chunks.iter().map(Vec::len).sum()
    }
}

#[async_trait::async_trait]
impl Provider for ChunkedMail {
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

    fn stream_email<'a>(
        &'a self,
        account: &'a AccountId,
        _cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        let total = self.total();
        let last = self.chunks.len().saturating_sub(1);
        Box::pin(async_stream::try_stream! {
            for (index, messages) in self.chunks.iter().enumerate() {
                // Optionally steal the lease right before this chunk's apply (once),
                // to force a mid-stream `StaleLease` and exercise restart-from-scratch.
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
                    // Each earlier chunk must already be committed and host-visible
                    // before this one is produced — what "streaming" buys the UI.
                    let visible = self
                        .store
                        .object_keys(&self.email_scope(account))
                        .await
                        .unwrap()
                        .len();
                    let expected: usize = self.chunks[..index].iter().map(Vec::len).sum();
                    assert_eq!(
                        visible, expected,
                        "chunk {index} was produced before earlier chunks committed"
                    );
                }
                let present: Vec<ProviderKey> =
                    messages.iter().map(|m| m.id.key().clone()).collect();
                // A first sync is a reconciling snapshot: intermediate chunks hold the
                // cursor and carry their present ids; the last tombstones and advances.
                if index == last {
                    yield EmailChunk::reconcile_last(
                        messages.clone(),
                        present,
                        Some(total),
                        self.cursor.clone(),
                    );
                } else {
                    yield EmailChunk::reconcile_page(messages.clone(), present, Some(total));
                }
            }
        })
    }
}

#[tokio::test]
async fn streamed_email_commits_each_chunk_and_reports_progress() {
    let store = Arc::new(SqliteStore::open_in_memory(clock()).unwrap());
    // Five messages over three chunks (2 + 2 + 1).
    let provider = ChunkedMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            vec![message("m1", "a", "One"), message("m2", "a", "Two")],
            vec![message("m3", "a", "Three"), message("m4", "a", "Four")],
            vec![message("m5", "a", "Five")],
        ],
        Arc::clone(&store),
        clock(),
    );

    // A closure observer records the running progress and the upserted keys per commit.
    let recorded: Commits = Mutex::new(Vec::new());
    let observer = |commit: &SyncCommit<'_>| {
        recorded.lock().unwrap().push((
            commit.fetched,
            commit.total,
            commit
                .upserted
                .iter()
                .map(|m| m.id.key().as_str().to_owned())
                .collect(),
        ));
    };
    let report = sync_mail_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::responsive(),
        &observer,
    )
    .await
    .unwrap();

    // Every message committed; the closing snapshot tombstoned nothing (the
    // accumulated present set covered them all).
    assert_eq!(report.email.upserted, 5);
    assert_eq!(report.email.tombstoned, 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 5);

    // Progress advanced 2 → 4 → 5, always against the known total of 5, and each
    // commit carried exactly the keys it upserted (the change events a host splices).
    let seq = recorded.lock().unwrap();
    assert_eq!(
        seq.iter().map(|(f, ..)| *f).collect::<Vec<_>>(),
        vec![2, 4, 5]
    );
    assert!(seq.iter().all(|(_, t, _)| *t == Some(5)));
    assert_eq!(seq[0].2, vec!["m1".to_owned(), "m2".to_owned()]);
    assert_eq!(seq[2].2, vec!["m5".to_owned()]);
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
    let provider = ChunkedMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![
            vec![message("m1", "a", "One"), message("m2", "a", "Two")],
            vec![message("m3", "a", "Three")],
        ],
        Arc::clone(&store),
        clock(),
    );

    let recorded: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    let observer = |commit: &SyncCommit<'_>| recorded.lock().unwrap().push(commit.fetched);
    // Exercise the depth-window builder alongside the fetch/chunk knobs.
    let tuning = StreamTuning::new(0, 0).within(engine_core::sync::SyncWindow::full());
    let applied = sync_email_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        tuning,
        &observer,
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
    // Progress reported per committed chunk (2 → 3).
    assert_eq!(*recorded.lock().unwrap(), vec![2, 3]);
}

#[tokio::test]
async fn streamed_resync_applies_a_delta() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    // First streamed sync lands the snapshot.
    sync_mail_streamed(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::default(),
        &IgnoreCommits,
    )
    .await
    .unwrap();
    // Second: a cursor now exists, so the email stream is a single empty delta.
    let report = sync_mail_streamed(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::default(),
        &IgnoreCommits,
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
    // Steal the lease right before chunk two — after chunk one has already committed.
    // The loop's chunk-two apply fails `StaleLease`, abandons the partial stream (the
    // cursor was never advanced for a reconcile pass), re-claims, and re-streams.
    let provider = ChunkedMail::new(
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

    let report = sync_mail_streamed(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::responsive(),
        &IgnoreCommits,
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
