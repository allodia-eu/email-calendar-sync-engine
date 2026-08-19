//! Streaming mail-sync tests: incremental chunk commit + host visibility, the
//! change events an observer sees, progress aggregation, the delta path, mid-stream
//! `StaleLease` restart, and resume-from-checkpoint after a killed cold sync. Uses
//! the shared fakes and helpers from the parent module via `use super::*`.

use engine_provider::EmailChunk;
use engine_store::{ApplyBatch, DerivedWrite, MailSelector};

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
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(self.caps)
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
    let report = sync_mail(
        core::slice::from_ref(&provider),
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::responsive(),
        &observer,
    )
    .await;

    // Every message committed; the closing snapshot tombstoned nothing (the
    // accumulated present set covered them all).
    assert_eq!(report.upserted(), 5);
    assert_eq!(report.tombstoned(), 0);
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
async fn an_account_pass_syncs_the_folder_list_then_its_folders() {
    // One call does both halves. They were two entrypoints, and a host had to drive them in the
    // right order itself; the report keeps them apart so a caller can still see which failed.
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
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

    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        report
            .mailboxes
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .upserted,
        1,
        "the folder"
    );
    assert_eq!(report.upserted(), 1, "the message");
    assert_eq!(report.folders.len(), 1);
    assert_eq!(report.folders_synced(), 1);
    assert_eq!(report.busy_scopes(), 0);
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
async fn streamed_resync_applies_a_delta() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    // First streamed sync lands the snapshot.
    sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::default(),
        &IgnoreCommits,
    )
    .await;
    // Second: a cursor now exists, so the email stream is a single empty delta.
    let report = sync_mail(
        core::slice::from_ref(&provider),
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::default(),
        &IgnoreCommits,
    )
    .await;
    assert_eq!(report.upserted(), 0);
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

    let report = sync_mail(
        core::slice::from_ref(&provider),
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::responsive(),
        &IgnoreCommits,
    )
    .await;

    assert!(
        provider.stolen.load(Ordering::SeqCst),
        "the steal must have run"
    );
    // The held cursor made the restart safe: all five land exactly once, none
    // duplicated or tombstoned by the abandoned partial pass.
    assert_eq!(report.upserted(), 5);
    assert_eq!(report.tombstoned(), 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 5);
}

/// An account pass repairs mail the v10 migration left in the graph with no thread.
///
/// No sequence of applies can produce that state — an apply threads what it stores, and a message
/// with no references still gets its own name — so it is written here directly, which is also how
/// it arises in the field: v10 fills the graph from stored payloads without assigning, so mail the
/// old whole-account pass had not yet grouped arrives graphed and ungrouped.
///
/// No arrival can undo it. The component lookup joins through the thread id a stored row already
/// carries, so a row with none is never a candidate to merge onto and stays alone for good.
#[tokio::test]
async fn an_account_pass_repairs_mail_the_migration_left_ungrouped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mailcal.sqlite");
    let store = SqliteStore::open(&path, clock()).unwrap();
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![],
    );
    let scope = provider.email_scope(&account());

    let mut original = message("m1", "a", "Original");
    original.envelope.message_id = vec![MessageIdHeader::new("root@h").unwrap()];
    let mut reply = message("m2", "a", "Re: Original");
    reply.envelope.message_id = vec![MessageIdHeader::new("reply@h").unwrap()];
    reply.envelope.references = vec![MessageIdHeader::new("root@h").unwrap()];
    let messages = [original, reply];

    let mut derived = DerivedWrite::empty();
    for message in &messages {
        derived.push_mail(engine_core::search_index::project_message(message));
    }
    let update = SyncUpdate::delta(messages.to_vec(), vec![]);
    let claim = store
        .claim_sync_scope(
            account(),
            &scope,
            LeaseRequest::new(worker(), Duration::from_mins(1)),
        )
        .await
        .unwrap();
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("c1")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    // What v10 leaves: the graph rows the apply wrote, and no thread on the rows they describe.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE message SET thread_id = NULL", [])
        .unwrap();
    assert!(
        store.has_ungrouped_graphed_mail(&account()).await.unwrap(),
        "the fixture must actually be the damaged state, or this proves nothing"
    );

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
        !store.has_ungrouped_graphed_mail(&account()).await.unwrap(),
        "a pass must repair it; leaving it strands the reply in its own conversation for good"
    );
    let rows = store
        .list_mail(&[account()], MailSelector::Newest, usize::MAX)
        .await
        .unwrap();
    let thread_of = |key: &str| {
        rows.iter()
            .find(|r| r.mail.key.as_str() == key)
            .unwrap()
            .mail
            .thread_id
            .clone()
    };
    assert_eq!(
        thread_of("m1"),
        thread_of("m2"),
        "the reply belongs with its original"
    );
    assert_eq!(thread_of("m1").unwrap().as_str(), "reply@h");
}
