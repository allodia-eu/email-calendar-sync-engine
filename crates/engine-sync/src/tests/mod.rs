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
    error::FailureClass,
    ids::{CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid},
    mail::{EmailAddress, MailStateChange, Mailbox, MailboxRole, Message},
    membership::Memberships,
    raw::RawIcal,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
    version::{ETag, RevisionTokens},
    write::{IdempotencyKey, PendingOp, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{
    CalendarWrites, Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, EventDeletion,
    EventDraft, EventEdit, EventPatch, EventRsvp, EventWrite, EventWriteReceipt, MailEdit,
    MailEditReceipt, MessageReport, OverrideSurvival, PatchTarget, Provider, ProviderError,
    ProviderResult, ReportReceipt, ReportVerdict, RsvpResponse, ScopeSync, SubmissionReceipt,
    WriteGuard,
};
use engine_recurrence::Horizon;
use engine_store::{
    ContactStore, LeaseRequest, ManualClock, PendingOpState, Store, StoreRead, WorkerId,
};
use store_sqlite::SqliteStore;

use super::{
    AccountId, AccountProgress, DrainOutcome, IgnoreCommits, StreamTuning, SyncCommit,
    SyncObserver, create_calendar_event, delete_calendar_event, drain_outbox, edit_mail,
    expand_calendar_horizon, patch_calendar_event, put_calendar_document,
    reconcile_calendar_events, refresh_folders, rsvp_calendar_event, submit_mail, sync_calendar,
    sync_mail,
};

mod calendar_sync;
mod calendar_write;
mod contact_sync;
mod drafts;
mod drain;
mod fake_provider;
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
    /// The send is throttled: retryable, so the op stays queued.
    Submit,
    /// The send is refused for good (a rejected recipient), so the op settles.
    PermanentSubmit,
    /// The send goes out, but the sender's copy cannot be filed in Sent.
    UnfiledCopy,
    /// Reporting a message is throttled.
    Report,
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
    /// Sends left to refuse before this provider starts accepting them: the outage a
    /// drain pass is supposed to ride out. Counts down per attempt.
    failing_sends: Mutex<u32>,
    /// Draft saves left to refuse, for the same reason.
    failing_drafts: Mutex<u32>,
    /// The `Message-ID` of every draft save that reached the provider, so a test can
    /// count the writes a queue of saves actually cost.
    draft_puts: Arc<Mutex<Vec<String>>>,
    /// Every draft removal that reached the provider.
    draft_deletes: Arc<Mutex<Vec<ProviderKey>>>,
}

impl FakeMail {
    fn new(mailboxes: Vec<Mailbox>, messages: Vec<Message>) -> Self {
        Self {
            caps: Capabilities::none()
                .with_mail()
                .with_mail_drafts()
                .with_submission()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Enforced, OverrideSurvival::kept()),
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
            failing_sends: Mutex::new(0),
            failing_drafts: Mutex::new(0),
            draft_puts: Arc::new(Mutex::new(Vec::new())),
            draft_deletes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Refuses the next `n` draft calls, so a test can queue a save with no network.
    fn failing_drafts(mut self, n: u32) -> Self {
        self.failing_drafts = Mutex::new(n);
        self
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

    /// Refuses the next `n` sends as throttled, then accepts: a provider that comes back.
    fn failing_sends(self, n: u32) -> Self {
        *self.failing_sends.lock().expect("failing_sends mutex") = n;
        self
    }

    /// Whether this send should be refused, counting one off the outage if so.
    fn send_is_out(&self) -> bool {
        let mut left = self.failing_sends.lock().expect("failing_sends mutex");
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }

    fn with_calendar(mut self, calendars: Vec<Calendar>, events: Vec<Event>) -> Self {
        self.calendars = calendars;
        self.events = events;
        self
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
