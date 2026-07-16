//! Client-scenario tests: a small **simulated email client** drives the `Engine`
//! façade end to end for the experiences a native app must nail:
//!
//! 1. **Cold add-account** — streams the newest mail first, commits it chunk by chunk (visible
//!    before the whole mailbox downloads), and — after a mid-sync "kill" — **resumes from where it
//!    stopped** instead of re-downloading.
//! 2. **Warm start** — paints cached mail instantly, *offline* included, with no provider call; a
//!    background sync then reconciles.
//! 3. **Live push** — a delta sync surfaces new mail immediately, and the change event carries the
//!    exact new rows so the client splices its list with no re-query.
//! 4. **Offline** — cached reads work; a sync degrades gracefully.
//!
//! Plus a **performance guard**: loading the initial page of cached mail stays well
//! under the 500 ms startup budget even for a large mailbox.
//!
//! `SimProvider` is a deterministic in-memory adapter that streams a resumable cold
//! backfill (its opaque cursor is the index of the next un-synced message), a delta
//! of "newly arrived" mail, and can be flipped offline or made to fail mid-stream.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

use engine_api::{
    AccountId, Engine, Message, StreamTuning, SyncApplied, SyncCommit, SyncScope, SyncWindow,
};
use engine_core::{
    ids::{MailboxId, MessageId, ProviderKey},
    mail::{Mailbox, MailboxRole},
    membership::Memberships,
    raw::RawMime,
    sync::{JmapDataType, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EmailChunk, EmailStream, Provider, ProviderError, ProviderResult,
    ScopeSync,
};

fn account() -> AccountId {
    AccountId::try_from("sim-acct").unwrap()
}

fn mailbox() -> Mailbox {
    let mut m = Mailbox::new(MailboxId::try_from("INBOX").unwrap(), "Inbox");
    m.role = Some(MailboxRole::Inbox);
    m
}

/// A dated message (newest have the latest date, so a windowed read ranks them first).
fn message(id: &str, subject: &str, date: &str) -> Message {
    let mut m = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("INBOX").unwrap()),
    );
    m.envelope.subject = Some(subject.to_owned());
    m.received_at = Some(date.parse().unwrap());
    m
}

/// `n` newest-first dated messages (`m0` newest), spread one minute apart.
fn messages(n: usize) -> Vec<Message> {
    (0..n)
        .map(|i| {
            // Descending timestamps so index 0 is the newest.
            let minute = 59 - (i % 60);
            let hour = 23 - ((i / 60) % 24);
            let date = format!("2026-06-15T{hour:02}:{minute:02}:00Z");
            message(&format!("m{i}"), &format!("Subject {i}"), &date)
        })
        .collect()
}

/// A deterministic streaming provider (see the module docs).
struct SimProvider {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    /// New mail a subsequent (post-backfill) delta will surface.
    arrivals: Mutex<Vec<Message>>,
    chunk: usize,
    offline: AtomicBool,
    /// Yield an error after committing this many backfill chunks (a simulated kill);
    /// `usize::MAX` never fails.
    fail_after: AtomicUsize,
    /// The resume index each `stream_email` call started from (proves resumption).
    starts: Mutex<Vec<usize>>,
}

impl SimProvider {
    fn new(messages: Vec<Message>, chunk: usize) -> Self {
        Self {
            caps: Capabilities::none().with_mail(),
            mailboxes: vec![mailbox()],
            messages,
            arrivals: Mutex::new(Vec::new()),
            chunk: chunk.max(1),
            offline: AtomicBool::new(false),
            fail_after: AtomicUsize::new(usize::MAX),
            starts: Mutex::new(Vec::new()),
        }
    }

    fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::SeqCst);
    }

    fn fail_after(&self, n: usize) {
        self.fail_after.store(n, Ordering::SeqCst);
    }

    fn deliver(&self, message: Message) {
        self.arrivals.lock().unwrap().push(message);
    }

    /// Builds the whole pass's chunks eagerly (the data is in memory), so the stream
    /// is a simple iterator — the orchestrator still commits and reports each one.
    fn build_chunks(&self, cursor: Option<&SyncState>) -> Vec<ProviderResult<EmailChunk>> {
        if self.offline.load(Ordering::SeqCst) {
            return vec![Err(ProviderError::retryable("account is offline"))];
        }
        let raw = cursor.map(SyncState::as_str);
        // Steady state: a delta of newly-arrived mail (additive, cursor stays "done").
        if raw == Some("done") {
            let arrivals = std::mem::take(&mut *self.arrivals.lock().unwrap());
            return vec![Ok(EmailChunk::additive(
                arrivals,
                Vec::new(),
                None,
                SyncState::new("done"),
            ))];
        }
        // Cold backfill (fresh, or resuming below a prior watermark), newest first.
        let start: usize = raw
            .and_then(|s| s.strip_prefix('b')?.parse().ok())
            .unwrap_or(0);
        self.starts.lock().unwrap().push(start);
        let total = self.messages.len();
        let fail_after = self.fail_after.load(Ordering::SeqCst);
        let mut out = Vec::new();
        let mut i = start;
        let mut committed = 0usize;
        while i < total {
            if committed == fail_after {
                out.push(Err(ProviderError::retryable("connection dropped mid-sync")));
                return out;
            }
            let end = (i + self.chunk).min(total);
            let batch = self.messages[i..end].to_vec();
            // The checkpoint each chunk advances to: the next index, or "done" at the end.
            let next = if end == total {
                SyncState::new("done")
            } else {
                SyncState::new(format!("b{end}"))
            };
            out.push(Ok(EmailChunk::additive(
                batch,
                Vec::new(),
                Some(total),
                next,
            )));
            committed += 1;
            i = end;
        }
        out
    }
}

#[async_trait::async_trait]
impl Provider for SimProvider {
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
        if self.offline.load(Ordering::SeqCst) {
            return Err(ProviderError::retryable("account is offline"));
        }
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
        Box::pin(futures_util::stream::iter(self.build_chunks(cursor)))
    }

    async fn fetch_message_source(
        &self,
        _account: &AccountId,
        _message: &Message,
    ) -> ProviderResult<RawMime> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(ProviderError::retryable("account is offline"));
        }
        Ok(RawMime::new(
            b"Content-Type: text/plain\r\n\r\nwarmed body".to_vec(),
        ))
    }
}

/// The client's in-memory mailbox view, updated purely from streamed change events —
/// what a native list-view binds to. Keyed by provider key so upserts replace.
#[derive(Default)]
struct ClientView {
    order: Vec<ProviderKey>,
    subjects: std::collections::HashMap<ProviderKey, String>,
}

impl ClientView {
    fn apply(&mut self, commit: &SyncCommit<'_>) {
        for message in commit.upserted {
            let key = message.id.key().clone();
            if !self.subjects.contains_key(&key) {
                self.order.push(key.clone());
            }
            self.subjects
                .insert(key, message.envelope.subject.clone().unwrap_or_default());
        }
        for key in commit.removed {
            self.order.retain(|k| k != key);
            self.subjects.remove(key);
        }
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

fn responsive() -> StreamTuning {
    // A large batch (few round trips) committed one message at a time — the "row as it
    // arrives" tuning an interactive client uses.
    StreamTuning::new(100, 1)
}

#[tokio::test]
async fn cold_add_streams_newest_first_and_resumes_after_a_kill() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(9), 3);
    // The app is "killed" after two committed chunks (six messages).
    provider.fail_after(2);

    let view = Mutex::new(ClientView::default());
    let observer = |commit: &SyncCommit<'_>| view.lock().unwrap().apply(commit);

    // First run: the streamed sync surfaces mail chunk by chunk, then the kill aborts it.
    let killed = engine
        .sync_mail_streamed(&provider, &account(), responsive(), &observer)
        .await;
    assert!(
        killed.is_err(),
        "the mid-stream failure surfaces as an error"
    );
    // Yet the six committed messages are durable and already in the client's view —
    // the newest first (m0 is the newest).
    assert_eq!(view.lock().unwrap().len(), 6, "committed rows are visible");
    assert_eq!(
        view.lock().unwrap().subjects.get(&key("m0")),
        Some(&"Subject 0".to_owned()),
        "the newest message rendered first"
    );

    // Resume: a fresh, healthy connection picks up from the checkpoint — it must be
    // handed the watermark cursor, not restart from the newest.
    let resumed = SimProvider::new(messages(9), 3);
    let report = engine
        .sync_mail_streamed(&resumed, &account(), responsive(), &observer)
        .await
        .expect("resume completes");
    assert_eq!(
        report.email.upserted, 3,
        "only the remaining three were fetched"
    );
    assert_eq!(
        *resumed.starts.lock().unwrap(),
        vec![6],
        "the resume started at the checkpoint (index 6), not 0"
    );
    // All nine are now present in the store and the client's view.
    assert_eq!(view.lock().unwrap().len(), 9);
    assert_eq!(
        engine
            .messages_windowed(&account(), 100)
            .await
            .unwrap()
            .len(),
        9
    );
}

#[tokio::test]
async fn warm_start_paints_cached_mail_offline_then_syncs() {
    // A prior session synced the account; a fresh session opens the same store.
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5), 2);
    engine
        .sync_mail_streamed(&provider, &account(), responsive(), &no_observer())
        .await
        .unwrap();

    // Now offline. The warm-start read still paints the cached mail with no provider
    // call — the instant, offline-first list.
    provider.set_offline(true);
    let cached = engine.messages_windowed(&account(), 50).await.unwrap();
    assert_eq!(cached.len(), 5, "cached mail renders offline");
    assert_eq!(engine.mailboxes(&account()).await.unwrap().len(), 1);

    // A background sync while offline degrades gracefully (an error the host ignores),
    // leaving the cached view intact.
    let offline_sync = engine
        .sync_mail_streamed(&provider, &account(), responsive(), &no_observer())
        .await;
    assert!(
        offline_sync.is_err(),
        "an offline sync fails, it does not corrupt state"
    );
    assert_eq!(
        engine
            .messages_windowed(&account(), 50)
            .await
            .unwrap()
            .len(),
        5
    );

    // Back online, a sync reconciles without disturbing the cache count.
    provider.set_offline(false);
    engine
        .sync_mail_streamed(&provider, &account(), responsive(), &no_observer())
        .await
        .unwrap();
    assert_eq!(
        engine
            .messages_windowed(&account(), 50)
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn live_push_surfaces_new_mail_immediately() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(3), 3);
    let view = Mutex::new(ClientView::default());
    let observer = |commit: &SyncCommit<'_>| view.lock().unwrap().apply(commit);

    // Initial sync fills the view.
    engine
        .sync_mail_streamed(&provider, &account(), responsive(), &observer)
        .await
        .unwrap();
    assert_eq!(view.lock().unwrap().len(), 3);

    // A watcher fires (new mail arrived). The host runs the scope's normal sync; the
    // delta commits the new message and the change event carries it, so the client
    // splices it in with no whole-list re-query.
    provider.deliver(message("m-new", "Fresh mail", "2026-06-16T09:00:00Z"));
    let applied: SyncApplied = engine
        .sync_folder_email_streamed(&provider, &account(), responsive(), &observer)
        .await
        .unwrap();
    assert_eq!(applied.upserted, 1);
    assert_eq!(
        view.lock().unwrap().len(),
        4,
        "the new message appeared immediately"
    );
    assert_eq!(
        view.lock().unwrap().subjects.get(&key("m-new")),
        Some(&"Fresh mail".to_owned())
    );
}

#[tokio::test]
async fn startup_loads_the_initial_page_well_under_500ms() {
    // A large cached mailbox (the perf-sensitive warm start). Seed it, then time the
    // initial-page read a host does on launch.
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5_000), 500);
    engine
        .sync_mail_streamed(&provider, &account(), StreamTuning::bulk(), &no_observer())
        .await
        .unwrap();

    let started = Instant::now();
    let page = engine.messages_windowed(&account(), 50).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(page.len(), 50, "the initial page of 50 loaded");
    assert!(
        elapsed.as_millis() < 500,
        "initial page load took {elapsed:?}, over the 500ms startup budget"
    );
}

#[tokio::test]
async fn missing_body_work_list_shrinks_as_a_warm_pass_fetches() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SimProvider::new(messages(5), 5);
    engine
        .sync_mail_streamed(&provider, &account(), responsive(), &no_observer())
        .await
        .unwrap();

    // A metadata-only sync leaves every body unwarmed — the work list is the whole
    // window, newest first (m0), same ranking as the windowed read.
    let missing = engine.messages_missing_body(&account(), 50).await.unwrap();
    assert_eq!(missing.len(), 5);
    assert_eq!(missing[0].envelope.subject.as_deref(), Some("Subject 0"));

    // Warm the two newest — the work list drops exactly those and keeps ranking.
    for message in &missing[..2] {
        engine
            .message_body(&provider, &account(), message)
            .await
            .unwrap();
    }
    let rest = engine.messages_missing_body(&account(), 50).await.unwrap();
    assert_eq!(rest.len(), 3);
    assert_eq!(rest[0].envelope.subject.as_deref(), Some("Subject 2"));

    // The cap keeps the newest *missing*, not just the newest.
    let capped = engine.messages_missing_body(&account(), 1).await.unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].envelope.subject.as_deref(), Some("Subject 2"));

    // A fully-warm window returns an empty work list.
    for message in &rest {
        engine
            .message_body(&provider, &account(), message)
            .await
            .unwrap();
    }
    assert!(
        engine
            .messages_missing_body(&account(), 50)
            .await
            .unwrap()
            .is_empty()
    );
}

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

/// A no-op observer for syncs whose progress a test does not inspect.
fn no_observer() -> impl engine_api::SyncObserver {
    engine_api::IgnoreCommits
}
