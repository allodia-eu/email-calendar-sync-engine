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

use engine_api::{AccountId, Engine, Message, StreamTuning, SyncCommit, SyncScope, SyncWindow};
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
        message: &Message,
    ) -> ProviderResult<RawMime> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(ProviderError::retryable("account is offline"));
        }
        // The key rides in a header, not the body: the blob area is content-addressed, so
        // sources identical across messages would dedupe to one file and hide anything a
        // test wants to say about per-message blobs — while the extracted body text (and
        // so the preview) must stay the same for every message.
        Ok(RawMime::new(
            format!(
                "Content-Type: text/plain\r\nX-Fixture-Key: {}\r\n\r\nwarmed body",
                message.id.key().as_str()
            )
            .into_bytes(),
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

#[path = "scenarios/cases.rs"]
mod cases;
#[path = "scenarios/size_cap.rs"]
mod size_cap;
