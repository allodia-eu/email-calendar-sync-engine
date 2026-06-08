//! Loop mechanics over a real [`SqliteStore`] driven by fake providers: container
//! and member persistence + indexing, empty-delta resync, and `StaleLease`
//! re-claim-and-recompute.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use core::num::NonZeroU32;

use engine_core::calendar::{
    Calendar, Event, Frequency, Recurrence, RecurrenceBound, RecurrenceRule,
};
use engine_core::ids::{
    CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid,
};
use engine_core::mail::{EmailAddress, Mailbox, MailboxRole, Message};
use engine_core::membership::Memberships;
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_core::time::{CalendarDateTime, LocalDateTime, TimeZoneId};
use engine_core::write::{IdempotencyKey, PendingOp, ResourceKey};
use engine_provider::{
    Capabilities, Draft, PageToken, Provider, ProviderError, ProviderResult, ScopeSync,
    SubmissionReceipt, SyncKind, SyncPage,
};
use engine_recurrence::Horizon;
use engine_store::{LeaseRequest, ManualClock, PendingOpState, Store, StoreRead, WorkerId};
use store_sqlite::SqliteStore;

use super::{
    AccountId, Duration, SyncProgress, submit_mail, sync_calendar, sync_mail, sync_mail_streamed,
};

mod streaming;

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
}

impl FakeMail {
    fn new(mailboxes: Vec<Mailbox>, messages: Vec<Message>) -> Self {
        Self {
            caps: Capabilities::none()
                .with_mail()
                .with_submission()
                .with_calendars(),
            mailboxes,
            messages,
            calendars: Vec::new(),
            events: Vec::new(),
            cursor: SyncState::new("cursor-1"),
            submit_fails: false,
        }
    }

    fn failing_submit(mut self) -> Self {
        self.submit_fails = true;
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
            self.cursor.clone(),
        ))
    }

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
        _page: Option<&PageToken>,
        _limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        // One page: a snapshot on first sync, an empty delta once a cursor exists.
        let (kind, changed, present, total) = if cursor.is_none() {
            let present: Vec<ProviderKey> =
                self.messages.iter().map(|m| m.id.key().clone()).collect();
            (
                SyncKind::Snapshot,
                self.messages.clone(),
                present,
                Some(self.messages.len()),
            )
        } else {
            (SyncKind::Delta, Vec::new(), Vec::new(), None)
        };
        Ok(SyncPage {
            kind,
            changed,
            removed: Vec::new(),
            present,
            next_page: None,
            next_cursor: self.cursor.clone(),
            total,
        })
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.submit_fails {
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

#[tokio::test]
async fn sync_mail_persists_containers_members_and_index() {
    let provider = FakeMail::new(
        vec![
            mailbox("a", "Inbox", Some(MailboxRole::Inbox)),
            mailbox("h", "Archive", None),
        ],
        vec![
            message("m1", "a", "Quarterly report"),
            message("m2", "a", "Lunch plans"),
        ],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(report.mailboxes.upserted, 2);
    assert_eq!(report.email.upserted, 2);

    // Containers landed under the mailbox scope.
    let mailbox_scope = provider.mailbox_scope(&account());
    assert_eq!(store.object_keys(&mailbox_scope).await.unwrap().len(), 2);

    // Members landed under the email scope, with derived index rows (searchable).
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 2);
    let counts = store
        .index_row_counts(&email_scope, &key("m1"))
        .await
        .unwrap();
    assert!(counts.fts >= 1, "expected a full-text row");
    assert!(counts.mail_index >= 1, "expected a scalar index row");
    assert!(counts.memberships >= 1, "expected a membership row");

    // The cursor advanced.
    let cursor = store
        .load_sync_state(account(), &email_scope)
        .await
        .unwrap();
    assert_eq!(cursor.as_ref().map(SyncState::as_str), Some("cursor-1"));
}

#[tokio::test]
async fn resync_with_cursor_applies_empty_delta() {
    let provider = FakeMail::new(
        vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
        vec![message("m1", "a", "Hello")],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    // Second run: a cursor now exists, so the fake returns an empty delta.
    let report = sync_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();
    assert_eq!(report.email.upserted, 0);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 1);
}

/// Wraps a [`FakeMail`] and, on the first email fetch, expires the loop's lease
/// (advancing the shared clock) then steals + releases the scope — forcing the
/// loop's apply to fail `StaleLease` and re-claim.
struct LeaseStealer {
    inner: FakeMail,
    store: Arc<SqliteStore<ManualClock>>,
    clock: ManualClock,
    stolen: AtomicBool,
}

#[async_trait::async_trait]
impl Provider for LeaseStealer {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
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

    async fn sync_email_page(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        if !self.stolen.swap(true, Ordering::SeqCst) {
            // Advance past the loop's lease TTL so its lease has expired, then
            // claim + release as another worker to bump the fencing generation.
            self.clock.advance(Duration::from_mins(2));
            let scope = self.inner.email_scope(account);
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
        self.inner
            .sync_email_page(account, cursor, page, limit)
            .await
    }
}

#[tokio::test]
async fn stale_lease_triggers_reclaim_and_recompute() {
    let clock = clock();
    let store = Arc::new(SqliteStore::open_in_memory(clock.clone()).unwrap());
    let provider = LeaseStealer {
        inner: FakeMail::new(
            vec![mailbox("a", "Inbox", Some(MailboxRole::Inbox))],
            vec![message("m1", "a", "Hello")],
        ),
        store: Arc::clone(&store),
        clock,
        stolen: AtomicBool::new(false),
    };

    // The loop's first email apply is stale (the steal bumped the generation during
    // fetch); it re-claims with the fresh state and recomputes to success.
    let report = sync_mail(
        &provider,
        &*store,
        &account(),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert!(
        provider.stolen.load(Ordering::SeqCst),
        "the steal must have run"
    );
    assert_eq!(report.email.upserted, 1);
    let email_scope = provider.email_scope(&account());
    assert_eq!(store.object_keys(&email_scope).await.unwrap().len(), 1);
}

#[tokio::test]
async fn submit_mail_enqueues_then_sends_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-1@test.local"),
    )
    .await
    .unwrap();

    assert_eq!(outcome.email_key.as_str(), "sent-1");
    assert_eq!(outcome.message_id.as_str(), "send-1@test.local");
    // The durable op reached terminal success.
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn submit_mail_records_failure_without_blind_retry() {
    let provider = FakeMail::new(vec![], vec![]).failing_submit();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = submit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        &draft("send-2@test.local"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, super::SyncError::Provider(_)));

    // Recover the op id via an idempotent re-enqueue and confirm it was recorded
    // Failed (not retried here).
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("submit:send-2@test.local").unwrap(),
                ResourceKey::new("draft:send-2@test.local").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[test]
fn draft_round_trips_through_a_durable_payload() {
    // The outbox stores the draft as a JSON payload; it must survive intact for a
    // recovery worker to re-submit it.
    let original = draft("durable@test.local");
    let payload = serde_json::to_value(&original).unwrap();
    let restored: Draft = serde_json::from_value(payload).unwrap();
    assert_eq!(restored, original);
}

fn calendar(id: &str, name: &str) -> Calendar {
    Calendar::new(CalendarId::try_from(id).unwrap(), name)
}

fn event(id: &str, uid: &str, calendar: &str, start: CalendarDateTime) -> Event {
    Event::new(
        EventId::try_from(id).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(CalendarId::try_from(calendar).unwrap()),
        start,
    )
}

fn zoned(year: i32, month: u8, day: u8, hour: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(year, month, day, hour, 0, 0).unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn year_horizon() -> Horizon {
    Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2026-12-31T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn sync_calendar_stores_containers_events_and_occurrences() {
    let single = event("evt-1", "uid-1@h", "work", zoned(2026, 3, 1, 9));
    let mut weekly = event("evt-2", "uid-2@h", "work", zoned(2026, 1, 5, 9));
    weekly.duration = "PT30M".parse().unwrap();
    let mut rule = RecurrenceRule::new(Frequency::Weekly);
    rule.bound = RecurrenceBound::Count(NonZeroU32::new(3).unwrap());
    weekly.recurrence = Some(Recurrence::from_rule(rule));

    let provider = FakeMail::new(vec![], vec![])
        .with_calendar(vec![calendar("work", "Work")], vec![single, weekly]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();
    assert_eq!(report.calendars.upserted, 1);
    assert_eq!(report.events.upserted, 2);

    let event_scope = provider.event_scope(&account());
    // Every event materializes occurrences: the single one once, the weekly-count-3
    // three times.
    assert_eq!(
        store
            .index_row_counts(&event_scope, &key("evt-1"))
            .await
            .unwrap()
            .occurrences,
        1
    );
    assert_eq!(
        store
            .index_row_counts(&event_scope, &key("evt-2"))
            .await
            .unwrap()
            .occurrences,
        3
    );
}

#[tokio::test]
async fn unsupported_recurrence_stores_event_without_occurrences() {
    let mut weird = event("evt-x", "uid-x@h", "work", zoned(2026, 3, 1, 9));
    // A sub-daily frequency is outside the expander's supported subset.
    weird.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
        Frequency::Hourly,
    )));
    let provider =
        FakeMail::new(vec![], vec![]).with_calendar(vec![calendar("work", "Work")], vec![weird]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let host_zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year_horizon(),
        &host_zone,
    )
    .await
    .unwrap();
    assert_eq!(report.events.upserted, 1);

    // The event is stored and indexed, but materializes no occurrences (rather than
    // failing the whole sync).
    let event_scope = provider.event_scope(&account());
    let counts = store
        .index_row_counts(&event_scope, &key("evt-x"))
        .await
        .unwrap();
    assert_eq!(counts.occurrences, 0);
    assert!(counts.event_index >= 1);
}
