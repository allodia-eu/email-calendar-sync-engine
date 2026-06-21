//! End-to-end facade tests: a host opens an [`Engine`] and syncs an account's
//! mail and calendar through a [`Provider`], exactly as a real host would. The
//! returned sync reports are the verification surface (search lands in a later
//! slice), and a failing provider must surface as [`ApiError::Sync`].

use engine_api::{AccountId, ApiError, Engine, Horizon, TimeZoneId};
use engine_core::calendar::{Calendar, Event};
use engine_core::ids::{CalendarId, EventId, MailboxId, MessageId, Uid};
use engine_core::mail::{Mailbox, MailboxRole, Message};
use engine_core::membership::Memberships;
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_core::time::{CalendarDateTime, LocalDateTime};
use engine_provider::{
    Capabilities, PageToken, Provider, ProviderError, ProviderResult, ScopeSync, SyncKind, SyncPage,
};

/// A minimal in-memory JMAP-shaped provider: a snapshot on the first sync of each
/// scope, configurable to fail its mail fetch.
struct FakeProvider {
    caps: Capabilities,
    mailboxes: Vec<Mailbox>,
    messages: Vec<Message>,
    calendars: Vec<Calendar>,
    events: Vec<Event>,
    fail: bool,
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
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new()
        }
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
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
        if self.fail {
            return Err(ProviderError::retryable("provider is offline"));
        }
        let present = self.mailboxes.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.mailboxes.clone(), present),
            SyncState::new("mbox-1"),
        ))
    }

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
        _page: Option<&PageToken>,
        _limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        let present = self.messages.iter().map(|m| m.id.key().clone()).collect();
        Ok(SyncPage {
            kind: SyncKind::Snapshot,
            changed: self.messages.clone(),
            removed: Vec::new(),
            present,
            next_page: None,
            next_cursor: SyncState::new("email-1"),
            total: Some(self.messages.len()),
        })
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        if self.fail {
            return Err(ProviderError::retryable("provider is offline"));
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
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let present = self.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(self.events.clone(), present),
            SyncState::new("evt-1"),
        ))
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

fn horizon() -> Horizon {
    Horizon::new(
        "2020-01-01T00:00:00Z".parse().unwrap(),
        "2030-01-01T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn syncs_mail_from_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let report = engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(report.mailboxes.upserted, 2);
    assert_eq!(report.email.upserted, 2);
}

#[tokio::test]
async fn syncs_calendar_from_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let report = engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(report.calendars.upserted, 1);
    assert_eq!(report.events.upserted, 1);
}

#[tokio::test]
async fn file_backed_engine_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");

    let first = Engine::open(&db).unwrap();
    first
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    drop(first);

    // Re-opening the same file re-runs migrations cleanly over the existing
    // database and syncs again against the stored cursor.
    let reopened = Engine::open(&db).unwrap();
    let report = reopened
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(report.email.upserted, 2);
}

#[tokio::test]
async fn mail_provider_failure_surfaces_as_a_sync_error() {
    let engine = Engine::open_in_memory().unwrap();
    let err = engine
        .sync_mail(&FakeProvider::failing(), &account())
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn calendar_provider_failure_surfaces_as_a_sync_error() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    let err = engine
        .sync_calendar(&FakeProvider::failing(), &account(), horizon(), &zone)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn open_rejects_an_unusable_path() {
    let dir = tempfile::tempdir().unwrap();
    // A database file under a directory that does not exist cannot be created.
    let bad = dir.path().join("missing").join("engine.sqlite");
    let err = Engine::open(&bad).unwrap_err();
    assert!(matches!(err, ApiError::Store(_)), "got {err:?}");
}
