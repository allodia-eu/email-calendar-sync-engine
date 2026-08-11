//! End-to-end facade tests: a host opens an `Engine` and syncs an account's mail
//! and calendar through a `Provider`, exactly as a real host would.
//!
//! The fake is **cursor-aware** — a full snapshot on the first sync of a scope, a
//! delta once a cursor exists — so the tests can assert real sync semantics from
//! the returned reports (search over the synced data is exercised too):
//! a snapshot upserts, a resync from a *persisted* cursor is an empty delta, and a
//! delta that drops a key tombstones it. Failures surface as `ApiError`, and two
//! concurrent syncs of one scope resolve to `ApiError::Busy`, not corruption.
//!
//! The cases live in the `sync/` submodules declared below; this binary holds the
//! shared providers, fixtures, and helpers they reach via `super::`.

use core::num::NonZeroU32;

use engine_api::{AccountId, Horizon};
use engine_core::{
    calendar::{Calendar, Event, Frequency, Recurrence, RecurrenceBound, RecurrenceRule},
    ids::{CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid},
    mail::{EmailAddress, Mailbox, MailboxRole, Message},
    membership::Memberships,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::{CalendarDateTime, LocalDateTime},
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, MailEdit, MailEditReceipt,
    Provider, ProviderError, ProviderResult, ScopeSync, SubmissionReceipt,
};
use tokio::sync::oneshot;

#[path = "sync/expansion.rs"]
mod expansion;
#[path = "sync/reads.rs"]
mod reads;
#[path = "sync/sync_lifecycle.rs"]
mod sync_lifecycle;
#[path = "sync/threading.rs"]
mod threading;
#[path = "sync/writes.rs"]
mod writes;

/// A minimal in-memory JMAP-shaped provider: a full snapshot on the first sync of a
/// scope (cursor `None`) and a delta afterwards. Configurable to fail its container
/// (mailbox/calendar) fetch, and to drop mail keys on a cursored resync.
struct FakeProvider {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    fail: bool,
    removed_on_resync: Vec<ProviderKey>,
    added_on_resync: Vec<Message>,
}

impl FakeProvider {
    fn new() -> Self {
        Self {
            caps: Capabilities::none().with_mail().with_calendars(),
            mailboxes: vec![
                mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
                mailbox("h", "Archive", None),
            ],
            messages: vec![
                message("m1", "a", "Quarterly report"),
                message("m2", "a", "Lunch plans"),
            ],
            calendars: vec![calendar("work", "Work")],
            events: vec![event("evt-1", "uid-1@h", "work")],
            fail: false,
            removed_on_resync: Vec::new(),
            added_on_resync: Vec::new(),
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new()
        }
    }

    /// On the next cursored resync, the email scope's delta drops `keys`.
    fn removing_on_resync(mut self, keys: Vec<ProviderKey>) -> Self {
        self.removed_on_resync = keys;
        self
    }

    /// On the next cursored resync, the email scope's delta adds `messages` — new mail
    /// arriving after the first sync already stored (and threaded) the rest.
    fn adding_on_resync(mut self, messages: Vec<Message>) -> Self {
        self.added_on_resync = messages;
        self
    }

    /// An IMAP-shaped provider: messages carry threading headers but no thread id, so
    /// the engine must derive one. `t2` replies to `t1` (shared id); `t3` is separate.
    fn threaded() -> Self {
        Self {
            messages: vec![
                threaded_message("t1", "a", "a@h", &[]),
                threaded_message("t2", "a", "b@h", &["a@h"]),
                threaded_message("t3", "a", "c@h", &[]),
            ],
            ..Self::new()
        }
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
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
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        if self.fail {
            return Err(ProviderError::retryable("provider is offline"));
        }
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("mbox-2"),
            ));
        }
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            SyncState::new("mbox-1"),
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
        // A cursored resync is an additive delta that adds and drops the configured
        // messages/keys; a first sync (no cursor) is a reconciling snapshot carrying
        // `present`, so the drain tombstones anything the server dropped.
        let chunk = if cursor.is_some() {
            EmailChunk::additive(
                self.added_on_resync.clone(),
                self.removed_on_resync.clone(),
                None,
                SyncState::new("email-2"),
            )
        } else {
            let present: Vec<ProviderKey> =
                self.messages.iter().map(|m| m.id.key().clone()).collect();
            EmailChunk::reconcile_last(
                self.messages.clone(),
                present,
                Some(self.messages.len()),
                SyncState::new("email-1"),
            )
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if self.fail {
            return Err(ProviderError::retryable("provider is offline"));
        }
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("cal-2"),
            ));
        }
        let present = self.calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.calendars.clone(), present),
            SyncState::new("cal-1"),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("evt-cursor-2"),
            ));
        }
        let present = self.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.events.clone(), present),
            SyncState::new("evt-cursor-1"),
        ))
    }
}

/// Wraps a [`FakeProvider`] and, inside `sync_mailboxes` (i.e. while the mailbox
/// scope's lease is held), signals `on_claim` then blocks on `until_release` — so a
/// test can deterministically hold a live lease while a second sync races for it.
struct GateProvider {
    inner: FakeProvider,
    on_claim: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    until_release: std::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Provider for GateProvider {
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
        // The lease is claimed and held by the time the fetch runs: announce it, then
        // park here (still holding it) until the racer has had its turn. Guards are
        // dropped before the await so the future stays `Send`.
        if let Some(tx) = self.on_claim.lock().expect("gate mutex").take() {
            let _ = tx.send(());
        }
        let release = self.until_release.lock().expect("gate mutex").take();
        if let Some(rx) = release {
            let _ = rx.await;
        }
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
}

/// Wraps a [`FakeProvider`] and overrides `submit_email` to succeed (filing the
/// sent copy under a fixed key, echoing the draft's `Message-ID`) or fail, so the
/// outbox-mediated submission facade can be exercised. Other methods delegate.
struct SubmittingProvider {
    inner: FakeProvider,
    fail: bool,
    /// Deliver, but report the sender's copy as unfiled (the two-step-transport case).
    unfiled: bool,
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
    ) -> ProviderResult<SubmissionReceipt> {
        if self.fail {
            return Err(ProviderError::retryable("smtp is offline"));
        }
        let key = ProviderKey::new("sent-1").unwrap();
        let id = draft.message_id.clone();
        if self.unfiled {
            let detail = "APPEND failed: connection reset";
            return Ok(SubmissionReceipt::unfiled(key, id, detail));
        }
        Ok(SubmissionReceipt::filed(key, id))
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
}

/// A mail provider whose snapshot drops the second message once `dropped` is set —
/// modeling a server-side removal (a move or expunge) that an IMAP-style delta cannot
/// report, so only a re-snapshot (after the cursor is cleared) reconciles it.
struct ReconcilingProvider {
    caps: Capabilities,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl Provider for ReconcilingProvider {
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
        let inbox = mailbox("a", "Inbox", Some(MailboxRole::Inbox));
        let present = [inbox.id.key().clone()].into_iter().collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(vec![inbox], present),
            SyncState::new("mbox-1"),
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
        // A cursor present → additive delta with no removals (the IMAP-no-CONDSTORE
        // baseline), so a plain resync never reconciles — only a re-snapshot does.
        let chunk = if cursor.is_some() {
            EmailChunk::additive(Vec::new(), Vec::new(), None, SyncState::new("email-2"))
        } else {
            // A snapshot: m2 is gone once the server "removed" it.
            let mut messages = vec![message("m1", "a", "First")];
            if !self.dropped.load(std::sync::atomic::Ordering::SeqCst) {
                messages.push(message("m2", "a", "Second"));
            }
            let present: Vec<ProviderKey> = messages.iter().map(|m| m.id.key().clone()).collect();
            let total = Some(messages.len());
            EmailChunk::reconcile_last(messages, present, total, SyncState::new("email-1"))
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").expect("valid account")
}

fn mailbox(id: &str, name: &str, role: Option<MailboxRole>) -> Mailbox {
    let mut mailbox = Mailbox::new(MailboxId::try_from(id).unwrap(), name);
    mailbox.role = role;
    mailbox
}

fn message(id: &str, mailbox: &str, subject: &str) -> Message {
    let mut message = Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from(mailbox).unwrap()),
    );
    message.envelope.subject = Some(subject.to_owned());
    message
}

fn threaded_message(id: &str, mailbox: &str, own: &str, references: &[&str]) -> Message {
    let mut message = message(id, mailbox, "subject");
    message.envelope.message_id = vec![MessageIdHeader::new(own).unwrap()];
    message.envelope.references = references
        .iter()
        .map(|value| MessageIdHeader::new(*value).unwrap())
        .collect();
    message
}

/// An inbox message with a delivery date and threading headers, for the windowed and thread
/// reads (its `received_at` becomes the mail index's sort date).
fn dated_message(id: &str, own: &str, references: &[&str], received: &str) -> Message {
    let mut message = threaded_message(id, "a", own, references);
    message.received_at = Some(received.parse().unwrap());
    message
}

fn calendar(id: &str, name: &str) -> Calendar {
    Calendar::new(CalendarId::try_from(id).unwrap(), name)
}

fn event(id: &str, uid: &str, calendar: &str) -> Event {
    Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from(calendar).unwrap()),
        CalendarDateTime::utc(LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap()),
    )
}

/// A weekly standup recurring `count` times from `start` (a UTC wall clock) — the
/// event whose instances only exist in the occurrence rows, never in `events()`.
fn weekly_event(id: &str, uid: &str, start: LocalDateTime, count: u32) -> Event {
    let mut event = Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        CalendarDateTime::utc(start),
    );
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(count).unwrap());
    event.recurrence = Some(Recurrence::from_rule(rule));
    event
}

fn horizon() -> Horizon {
    Horizon::new(
        "2020-01-01T00:00:00Z".parse().unwrap(),
        "2030-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

fn draft(message_id: &str, subject: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        subject,
        "see attached",
    )
}
