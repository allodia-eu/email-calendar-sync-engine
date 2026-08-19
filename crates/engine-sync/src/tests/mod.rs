//! Shared scaffolding for the sync-loop integration tests: the configurable
//! in-memory `FakeMail` provider and the small fixture helpers (accounts, clocks,
//! workers, mailboxes, messages, drafts, provider keys). The behavior tests live in
//! the themed submodules and reach this scaffolding via `use super::*`.

use core::{num::NonZeroU32, time::Duration};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use engine_core::{
    calendar::{Calendar, Event, Frequency, Recurrence, RecurrenceBound, RecurrenceRule},
    ids::{CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid},
    mail::{EmailAddress, MailStateChange, Mailbox, MailboxRole, Message},
    membership::Memberships,
    raw::RawIcal,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
    version::{ETag, RevisionTokens},
    write::{IdempotencyKey, PendingOp, ResourceKey},
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, EventDeletion, EventDraft,
    EventEdit, EventPatch, EventRsvp, EventWrite, EventWriteReceipt, MailEdit, MailEditReceipt,
    PatchTarget, Provider, ProviderError, ProviderResult, RsvpResponse, ScopeSync,
    SubmissionReceipt, WriteGuard,
};
use engine_recurrence::Horizon;
use engine_store::{
    ContactStore, LeaseRequest, ManualClock, PendingOpState, Store, StoreRead, WorkerId,
};
use store_sqlite::SqliteStore;

use super::{
    AccountId, AccountProgress, IgnoreCommits, StreamTuning, SyncCommit, SyncObserver,
    create_calendar_event, delete_calendar_event, edit_mail, expand_calendar_horizon,
    patch_calendar_event, put_calendar_document, reconcile_calendar_events, refresh_folders,
    rsvp_calendar_event, submit_mail, sync_calendar, sync_mail,
};

mod calendar_sync;
mod calendar_write;
mod contact_sync;
mod mail_account;
mod mail_edit;
mod mail_sync;
mod state_change;
mod streaming;
mod streaming_resume;
mod submit;

/// A way the fake provider can fail, so a test can drive one failure path without the
/// provider carrying a flag per path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The send fails outright.
    Submit,
    /// The send is lost *after* `DATA` — the ambiguous, unretryable case.
    AmbiguousSubmit,
    /// Every write's revision guard is refused (a CalDAV `412`, a JMAP `stateMismatch`).
    WriteGuard,
    /// The calendar-container fetch fails — so a pass that touches the container scope
    /// cannot succeed, and one that is events-only cannot notice.
    CalendarFetch,
}

/// A configurable in-memory mail provider: a snapshot on first sync, an empty
/// delta once a cursor exists.
struct FakeMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    cursor: SyncState,
    faults: Vec<Fault>,
    /// Emitted once, as an additive delta, on the first pass that finds a cursor —
    /// the shape a flag change arrives in. Empty means "nothing changed".
    delta: Mutex<Vec<Message>>,
    /// Keyword-only changes emitted once alongside `delta` — what a provider that can tell a
    /// mark-read from a content change sends.
    state_delta: Mutex<Vec<MailStateChange>>,
    /// The folder this provider is bound to, for the IMAP shape where a host builds one per
    /// folder. `None` is the JMAP shape: one provider, one account-wide email scope.
    folder: Option<MailboxId>,
    /// Records, in order, the scopes whose mail was fetched — so a test can assert *which folder
    /// went first*, which a report ordered by completion cannot show.
    started: Arc<Mutex<Vec<MailboxId>>>,
}

impl FakeMail {
    fn new(mailboxes: Vec<Mailbox>, messages: Vec<Message>) -> Self {
        Self {
            caps: Capabilities::none()
                .with_mail()
                .with_submission()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Enforced),
            mailboxes,
            messages,
            calendars: Vec::new(),
            events: Vec::new(),
            cursor: SyncState::new("cursor-1"),
            faults: Vec::new(),
            delta: Mutex::default(),
            state_delta: Mutex::default(),
            folder: None,
            started: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Binds this provider to one folder — the IMAP shape, where a host builds one per folder
    /// and the engine fans them out. `log` is shared across the set so the order they were
    /// *started* in is visible to the test.
    fn in_folder(mut self, mailbox: &str, log: &Arc<Mutex<Vec<MailboxId>>>) -> Self {
        self.folder = Some(MailboxId::try_from(mailbox).unwrap());
        self.started = Arc::clone(log);
        self
    }

    /// Arms the keyword-only changes this provider emits after its first (snapshot) pass —
    /// the shape a mark-read arrives in once the adapter can recognise one.
    fn then_changing_state(self, changes: Vec<MailStateChange>) -> Self {
        *self
            .state_delta
            .lock()
            .expect("keyword delta mutex poisoned") = changes;
        self
    }

    fn failing(mut self, fault: Fault) -> Self {
        self.faults.push(fault);
        self
    }

    fn fails(&self, fault: Fault) -> bool {
        self.faults.contains(&fault)
    }

    fn with_calendar(mut self, calendars: Vec<Calendar>, events: Vec<Event>) -> Self {
        self.calendars = calendars;
        self.events = events;
        self
    }
}

#[async_trait::async_trait]
impl Provider for FakeMail {
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
        match &self.folder {
            Some(mailbox) => SyncScope::ImapMailbox {
                account: account.clone(),
                mailbox: mailbox.clone(),
            },
            None => SyncScope::JmapType {
                account: account.clone(),
                data_type: JmapDataType::Email,
            },
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
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
        if let Some(folder) = &self.folder {
            self.started.lock().unwrap().push(folder.clone());
        }
        // One chunk: a reconciling snapshot on first sync (so the drain tombstones),
        // an additive empty delta once a cursor exists.
        let chunk = if cursor.is_none() {
            let present: Vec<ProviderKey> =
                self.messages.iter().map(|m| m.id.key().clone()).collect();
            EmailChunk::reconcile_last(
                self.messages.clone(),
                present,
                Some(self.messages.len()),
                self.cursor.clone(),
            )
        } else {
            let changed = core::mem::take(&mut *self.delta.lock().expect("delta mutex poisoned"));
            let keywords = core::mem::take(
                &mut *self
                    .state_delta
                    .lock()
                    .expect("keyword delta mutex poisoned"),
            );
            EmailChunk::additive(changed, Vec::new(), None, self.cursor.clone())
                .with_patched(keywords)
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.fails(Fault::AmbiguousSubmit) {
            Err(ProviderError::needs_confirmation(
                "post-DATA acknowledgement lost",
            ))
        } else if self.fails(Fault::Submit) {
            Err(ProviderError::rate_limited("slow down", None))
        } else {
            Ok(SubmissionReceipt::filed(
                ProviderKey::new("sent-1").unwrap(),
                draft.message_id.clone(),
            ))
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if self.fails(Fault::CalendarFetch) {
            return Err(ProviderError::retryable("calendar list unreachable"));
        }
        let present = self.calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.calendars.clone(), present),
            self.cursor.clone(),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let present = self.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.events.clone(), present),
            self.cursor.clone(),
        ))
    }

    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("an event already exists there"));
        }
        // Mints the id the way a CalDAV adapter does — from the caller's UID. (A JMAP
        // adapter would return a server-assigned one; the driver cannot tell, which is
        // the point.)
        Ok(EventWriteReceipt::new(
            EventId::try_from(format!("/cal/{}.ics", draft.uid.as_str()).as_str()).unwrap(),
            draft.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        _base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            // A failed revision guard: a CalDAV `412`, or a JMAP `stateMismatch`.
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(EventWriteReceipt::new(
            edit.event.clone(),
            edit.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(EventWriteReceipt::new(
            write.event.clone(),
            write.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        _base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        // Stands in for every adapter's refusal of a control it cannot honour — the
        // driver must record and surface it, not swallow it.
        if rsvp.comment.is_some() {
            return Err(ProviderError::invalid_state("no note on this transport"));
        }
        Ok(EventWriteReceipt::new(
            rsvp.event.clone(),
            rsvp.uid.clone(),
            RevisionTokens::from_etag(ETag::new("\"put-v1\"")),
        ))
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        _deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        if self.fails(Fault::WriteGuard) {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(())
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        if self.fails(Fault::WriteGuard) {
            // The IMAP analogue of a CalDAV 412: a stale UID under a changed
            // UIDVALIDITY (`imap-smtp.md`) — recompute after a re-sync.
            return Err(ProviderError::conflict("UIDVALIDITY changed"));
        }
        Ok(MailEditReceipt::new(edit.target().clone()))
    }
}

fn draft(message_id: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
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

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn clock() -> ManualClock {
    ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap())
}

fn worker() -> WorkerId {
    WorkerId::new("w-1")
}

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}
