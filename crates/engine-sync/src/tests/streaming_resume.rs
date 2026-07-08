//! Resumable-backfill + progress-aggregation streaming tests: a killed cold sync
//! resuming from its checkpoint, and the account-level progress aggregator. Split
//! from `streaming.rs` to keep each file within the size limit; shares the parent
//! module's fakes and helpers via `use super::*`.

use engine_provider::EmailChunk;

use super::*;

/// A provider that models a **resumable cold backfill**: a fixed newest-first list
/// of messages, streamed in `chunk` sizes, where the opaque cursor is the index of
/// the next un-synced message. Each additive chunk checkpoints that index, so a
/// restart with the checkpointed cursor resumes from there rather than re-fetching.
/// It records the start index of every `stream_email` call, proving resumption.
struct BackfillMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    chunk: usize,
    starts: Arc<Mutex<Vec<usize>>>,
    stop_after: Option<usize>,
}

impl BackfillMail {
    fn new(messages: Vec<Message>, chunk: usize) -> Self {
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes: vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
            messages,
            chunk,
            starts: Arc::new(Mutex::new(Vec::new())),
            stop_after: None,
        }
    }

    /// Emit at most `n` chunks before ending the stream early (simulating a kill).
    fn stopping_after(mut self, n: usize) -> Self {
        self.stop_after = Some(n);
        self
    }
}

#[async_trait::async_trait]
impl Provider for BackfillMail {
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
            SyncState::new("mbox"),
        ))
    }

    fn stream_email<'a>(
        &'a self,
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        // The cursor is the index of the next un-synced message; `None` starts at 0.
        let start: usize = cursor.map_or(0, |c| c.as_str().parse().unwrap());
        self.starts.lock().unwrap().push(start);
        let total = self.messages.len();
        let chunk = self.chunk.max(1);
        let stop_after = self.stop_after;
        Box::pin(async_stream::try_stream! {
            let mut offset = start;
            let mut emitted = 0usize;
            while offset < total {
                if stop_after == Some(emitted) {
                    // A simulated kill: end the stream without reaching the floor. The
                    // last committed chunk's checkpoint is the durable resume point.
                    break;
                }
                let end = (offset + chunk).min(total);
                let batch: Vec<Message> = self.messages[offset..end].to_vec();
                offset = end;
                emitted += 1;
                // Additive backfill: checkpoint the next index on every chunk.
                let checkpoint = SyncState::new(offset.to_string());
                yield EmailChunk::additive(batch, Vec::new(), Some(total), checkpoint);
            }
        })
    }
}

#[tokio::test]
async fn cold_backfill_resumes_from_the_checkpoint_after_a_kill() {
    let clock = clock();
    let store = Arc::new(SqliteStore::open_in_memory(clock.clone()).unwrap());
    // Nine messages, chunked by three.
    let messages: Vec<Message> = (1..=9)
        .map(|n| message(&format!("m{n}"), "a", "s"))
        .collect();

    // First run is "killed" after two chunks (six messages committed, cursor = 6).
    let killed = BackfillMail::new(messages.clone(), 3).stopping_after(2);
    let applied = sync_email_streamed(
        &killed,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(3, 3),
        &IgnoreCommits,
    )
    .await
    .unwrap();
    assert_eq!(applied.upserted, 6);
    let email_scope = killed.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 6);

    // Second run resumes: it must be handed the checkpointed cursor (index 6), NOT
    // start over from the newest — the whole point of resumable backfill.
    let resumed = BackfillMail::new(messages, 3);
    let applied = sync_email_streamed(
        &resumed,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(3, 3),
        &IgnoreCommits,
    )
    .await
    .unwrap();
    // Only the remaining three were fetched, and the stream started at index 6.
    assert_eq!(applied.upserted, 3);
    assert_eq!(*resumed.starts.lock().unwrap(), vec![6]);
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 9);
}

#[tokio::test]
async fn account_progress_aggregates_two_folders() {
    // Two folders streamed into one aggregator: the total stays indeterminate until
    // both have reported, then reads their sum — the account-level bar a host renders.
    let progress = AccountProgress::new(2);
    let inbox = SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    };
    let sent = SyncScope::ImapMailbox {
        account: account(),
        mailbox: MailboxId::try_from("Sent").unwrap(),
    };
    progress.committed(&SyncCommit {
        scope: &inbox,
        fetched: 3,
        total: Some(10),
        upserted: &[],
        removed: &[],
        tombstoned: 0,
    });
    assert_eq!(progress.snapshot().total, None);
    progress.committed(&SyncCommit {
        scope: &sent,
        fetched: 4,
        total: Some(5),
        upserted: &[],
        removed: &[],
        tombstoned: 0,
    });
    let snap = progress.snapshot();
    assert_eq!(snap.fetched, 7);
    assert_eq!(snap.total, Some(15));
}
