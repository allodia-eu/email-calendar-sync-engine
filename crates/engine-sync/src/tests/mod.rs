//! Shared scaffolding for the sync-loop integration tests: the configurable
//! in-memory `FakeMail` provider and the small fixture helpers (accounts, clocks,
//! workers, mailboxes, messages, drafts, provider keys). The behavior tests live in
//! the themed submodules and reach this scaffolding via `use super::*`.

use core::num::NonZeroU32;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use engine_core::{
    calendar::{Calendar, Event, Frequency, Recurrence, RecurrenceBound, RecurrenceRule},
    ids::{CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid},
    mail::{EmailAddress, Mailbox, MailboxRole, Message},
    membership::Memberships,
    raw::RawIcal,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
    version::ETag,
    write::{IdempotencyKey, PendingOp, ResourceKey},
};
use engine_provider::{
    Capabilities, ConnectionInfo, Draft, EmailChunk, EmailStream, EventDeletion, EventWrite,
    EventWriteReceipt, MailEdit, MailEditReceipt, Provider, ProviderError, ProviderResult,
    ScopeSync, SubmissionReceipt,
};
use engine_recurrence::Horizon;
use engine_store::{LeaseRequest, ManualClock, PendingOpState, Store, StoreRead, WorkerId};
use store_sqlite::SqliteStore;

use super::{
    AccountId, AccountProgress, Duration, IgnoreCommits, StreamTuning, SyncCommit, SyncObserver,
    delete_calendar_event, edit_mail, submit_mail, sync_calendar, sync_email_streamed, sync_mail,
    sync_mail_streamed, sync_mailbox_list, write_calendar_event,
};

mod calendar_sync;
mod calendar_write;
mod mail_edit;
mod mail_sync;
mod streaming;
mod streaming_resume;
mod submit;

/// A configurable in-memory mail provider: a snapshot on first sync, an empty
/// delta once a cursor exists.
struct FakeMail {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    cursor: SyncState,
    submit_fails: bool,
    submit_ambiguous: bool,
    write_conflicts: bool,
}

impl FakeMail {
    fn new(mailboxes: Vec<Mailbox>, messages: Vec<Message>) -> Self {
        Self {
            caps: Capabilities::none()
                .with_mail()
                .with_submission()
                .with_calendars()
                .with_calendar_writes(),
            mailboxes,
            messages,
            calendars: Vec::new(),
            events: Vec::new(),
            cursor: SyncState::new("cursor-1"),
            submit_fails: false,
            submit_ambiguous: false,
            write_conflicts: false,
        }
    }

    fn failing_submit(mut self) -> Self {
        self.submit_fails = true;
        self
    }

    fn ambiguous_submit(mut self) -> Self {
        self.submit_ambiguous = true;
        self
    }

    fn conflicting_writes(mut self) -> Self {
        self.write_conflicts = true;
        self
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
        _account: &'a AccountId,
        cursor: Option<&'a SyncState>,
        _window: SyncWindow,
        _fetch_batch: usize,
        _chunk_size: usize,
    ) -> EmailStream<'a> {
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
            EmailChunk::additive(Vec::new(), Vec::new(), None, self.cursor.clone())
        };
        Box::pin(futures_util::stream::iter(vec![Ok(chunk)]))
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.submit_ambiguous {
            Err(ProviderError::needs_confirmation(
                "post-DATA acknowledgement lost",
            ))
        } else if self.submit_fails {
            Err(ProviderError::rate_limited("slow down", None))
        } else {
            Ok(SubmissionReceipt::new(
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

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        if self.write_conflicts {
            // A failed If-Match/If-None-Match precondition (RFC 4791 §5.3.2).
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(EventWriteReceipt::new(
            write.href.key().clone(),
            write.uid.clone(),
            Some(ETag::new("\"put-v1\"")),
        ))
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        _deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        if self.write_conflicts {
            return Err(ProviderError::conflict("etag precondition failed"));
        }
        Ok(())
    }

    async fn edit_mail(
        &self,
        _account: &AccountId,
        edit: &MailEdit,
    ) -> ProviderResult<MailEditReceipt> {
        if self.write_conflicts {
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
