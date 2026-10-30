//! A host changes how far back an account's mail goes by clearing its mail cursors and syncing
//! again under the new window. Neither half may be undone by a pass that started under the old
//! one: a pass already running stops rather than write its cursor back over the clear, and a
//! cursor written under a later floor than a pass asks for is not resumed from.

use std::sync::atomic::AtomicUsize;

use engine_core::time::CalendarDate;
use engine_provider::{CalendarWrites, EmailChunk};

use super::*;

/// The lease every pass here claims under, longer than any pass here runs.
const TTL: Duration = Duration::from_mins(1);

/// How far apart a paced backfill's chunks arrive.
const EVERY: Duration = Duration::from_secs(5);

/// A backfill of `chunks` messages, one every [`EVERY`], that remembers each cursor it was asked
/// to resume from. Resumed from a cursor, it has nothing new and only moves the cursor on.
struct Backfill {
    mailboxes: Vec<Mailbox>,
    chunks: usize,
    paced: bool,
    resumed_from: Mutex<Vec<Option<String>>>,
    started: AtomicUsize,
}

impl Backfill {
    fn new(chunks: usize, paced: bool) -> Self {
        Self {
            mailboxes: vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
            chunks,
            paced,
            resumed_from: Mutex::new(Vec::new()),
            started: AtomicUsize::new(0),
        }
    }

    fn resumed_from(&self) -> Vec<Option<String>> {
        self.resumed_from.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Provider for Backfill {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail())
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
        self.started.fetch_add(1, Ordering::SeqCst);
        self.resumed_from
            .lock()
            .unwrap()
            .push(cursor.map(|c| c.as_str().to_owned()));
        let resumed = cursor.is_some();
        Box::pin(async_stream::try_stream! {
            if resumed {
                yield EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("delta"));
                return;
            }
            for n in 0..self.chunks {
                if self.paced {
                    tokio::time::sleep(EVERY).await;
                }
                let batch = vec![message(&format!("m{n}"), "a", "s")];
                yield EmailChunk::additive(batch, Vec::new(), None, SyncState::new(n.to_string()));
            }
        })
    }
}

impl engine_provider::MailboxWrites for Backfill {}
impl CalendarWrites for Backfill {}

async fn pass(provider: &Backfill, store: &SqliteStore<ManualClock>, window: SyncWindow) {
    sync_mail(
        core::slice::from_ref(provider),
        store,
        &account(),
        worker(),
        TTL,
        StreamTuning::new(1, 1).within(window),
        &IgnoreCommits,
    )
    .await;
}

fn since(month: u8) -> SyncWindow {
    SyncWindow::since(CalendarDate::new(2026, month, 1).unwrap())
}

#[tokio::test(start_paused = true)]
async fn a_cleared_cursor_stops_the_pass_that_held_it() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let provider = Backfill::new(6, true);
    let scope = provider.email_scope(&account());

    let host = async {
        // Two chunks in, the host changes the window and clears the cursor.
        tokio::time::sleep(EVERY * 5 / 2).await;
        store.clear_scope_cursor(&scope).await.unwrap();
    };
    tokio::join!(pass(&provider, &store, since(4)), host);

    assert_eq!(
        provider.started.load(Ordering::SeqCst),
        1,
        "the pass started over under the window the host had just left"
    );
    assert_eq!(
        store.load_sync_state(account(), &scope).await.unwrap(),
        None,
        "the pass wrote its cursor back over the clear"
    );
    assert_eq!(store.object_keys(&scope).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_pass_asking_for_older_mail_than_its_cursor_covers_starts_over() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let provider = Backfill::new(2, false);

    pass(&provider, &store, since(4)).await;
    // A floor a month later is what a host computing it from today passes a month on.
    pass(&provider, &store, since(5)).await;
    pass(&provider, &store, since(3)).await;
    pass(&provider, &store, SyncWindow::full()).await;
    pass(&provider, &store, SyncWindow::full()).await;

    assert_eq!(
        provider.resumed_from(),
        vec![None, Some("1".to_owned()), None, None, Some("1".to_owned())],
    );
}
