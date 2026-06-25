//! End-to-end facade tests: a host opens an [`Engine`] and syncs an account's mail
//! and calendar through a [`Provider`], exactly as a real host would.
//!
//! The fake is **cursor-aware** — a full snapshot on the first sync of a scope, a
//! delta once a cursor exists — so the tests can assert real sync semantics from
//! the returned reports (search over the synced data is exercised below):
//! a snapshot upserts, a resync from a *persisted* cursor is an empty delta, and a
//! delta that drops a key tombstones it. Failures surface as [`ApiError`], and two
//! concurrent syncs of one scope resolve to [`ApiError::Busy`], not corruption.

use engine_api::{
    AccountId, ApiError, Engine, Horizon, PendingOpId, PendingOpState, SyncProgress, TimeZoneId,
};
use engine_core::calendar::{Calendar, Event};
use engine_core::ids::{
    CalendarId, EventId, MailboxId, MessageId, MessageIdHeader, ProviderKey, Uid,
};
use engine_core::mail::{EmailAddress, Mailbox, MailboxRole, Message};
use engine_core::membership::Memberships;
use engine_core::sync::{JmapDataType, SyncScope, SyncState, SyncUpdate};
use engine_core::time::{CalendarDateTime, LocalDateTime};
use engine_provider::{
    Capabilities, Draft, PageToken, Provider, ProviderError, ProviderResult, ScopeSync,
    SubmissionReceipt, SyncKind, SyncPage,
};
use tokio::sync::oneshot;

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

    async fn sync_email_page(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
        _page: Option<&PageToken>,
        _limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        if cursor.is_some() {
            // A cursored resync: a delta that adds nothing and drops any configured keys.
            return Ok(SyncPage {
                kind: SyncKind::Delta,
                changed: Vec::new(),
                removed: self.removed_on_resync.clone(),
                present: Vec::new(),
                next_page: None,
                next_cursor: SyncState::new("email-2"),
                total: None,
            });
        }
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

    async fn sync_email_page(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
        page: Option<&PageToken>,
        limit: usize,
    ) -> ProviderResult<SyncPage<Message>> {
        self.inner
            .sync_email_page(account, cursor, page, limit)
            .await
    }
}

/// Wraps a [`FakeProvider`] and overrides `submit_email` to succeed (filing the
/// sent copy under a fixed key, echoing the draft's `Message-ID`) or fail, so the
/// outbox-mediated submission facade can be exercised. Other methods delegate.
struct SubmittingProvider {
    inner: FakeProvider,
    fail: bool,
}

#[async_trait::async_trait]
impl Provider for SubmittingProvider {
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
        self.inner
            .sync_email_page(account, cursor, page, limit)
            .await
    }

    async fn submit_email(
        &self,
        _account: &AccountId,
        draft: &Draft,
    ) -> ProviderResult<SubmissionReceipt> {
        if self.fail {
            return Err(ProviderError::retryable("smtp is offline"));
        }
        Ok(SubmissionReceipt::new(
            ProviderKey::new("sent-1").unwrap(),
            draft.message_id.clone(),
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
    // First sync is a snapshot: both containers and both members are upserted.
    assert_eq!(report.mailboxes.upserted, 2);
    assert_eq!(report.email.upserted, 2);
    assert_eq!(report.email.tombstoned, 0);
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
async fn reopen_resumes_mail_from_the_persisted_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");

    let first = Engine::open(&db).unwrap();
    let initial = first
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(initial.email.upserted, 2); // first sync is a snapshot
    drop(first);

    // Reopen and sync again. Because the cursor persisted, the fake is asked for a
    // *delta* and returns an empty one — so nothing is upserted. On a fresh/lost DB
    // there would be no cursor, the fake would return a snapshot, and upserted would
    // be 2. Asserting 0 is therefore a real persistence check, not a re-apply count.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();
    assert_eq!(resumed.email.upserted, 0);
    assert_eq!(resumed.email.tombstoned, 0);
}

#[tokio::test]
async fn reopen_resumes_calendar_from_the_persisted_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();

    let first = Engine::open(&db).unwrap();
    let initial = first
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(initial.events.upserted, 1);
    drop(first);

    // Same persistence check for the on-disk calendar/event/occurrence path: the
    // resumed sync is an empty delta off the persisted cursor.
    let reopened = Engine::open(&db).unwrap();
    let resumed = reopened
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();
    assert_eq!(resumed.events.upserted, 0);
}

#[tokio::test]
async fn resync_tombstones_mail_dropped_from_the_delta() {
    let engine = Engine::open_in_memory().unwrap();
    // m1's stored key is its MessageId's provider key — recompute it from the same id.
    let dropped = message("m1", "a", "Quarterly report").id.key().clone();
    let provider = FakeProvider::new().removing_on_resync(vec![dropped]);

    let initial = engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(initial.email.upserted, 2);

    // The cursor now exists, so the second sync is a delta that drops m1: it must be
    // tombstoned, with nothing upserted.
    let resync = engine.sync_mail(&provider, &account()).await.unwrap();
    assert_eq!(resync.email.tombstoned, 1);
    assert_eq!(resync.email.upserted, 0);
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
async fn concurrent_same_scope_sync_reports_busy() {
    let engine = Engine::open_in_memory().unwrap();
    let acct = account();
    let (claim_tx, claim_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let gate = GateProvider {
        inner: FakeProvider::new(),
        on_claim: std::sync::Mutex::new(Some(claim_tx)),
        until_release: std::sync::Mutex::new(Some(release_rx)),
    };

    // The gated sync claims the mailbox scope and parks (lease held) until released.
    let held = engine.sync_mail(&gate, &acct);
    // The racer waits until the lease is held, then attempts the same scope.
    let racer = async {
        claim_rx.await.expect("first sync should claim the scope");
        let outcome = engine.sync_mail(&FakeProvider::new(), &acct).await;
        release_tx.send(()).expect("first sync still parked");
        outcome
    };

    let (held_result, racer_result) = tokio::join!(held, racer);
    held_result.expect("the lease holder completes once released");

    // The racer found the scope's lease live -> retryable ScopeHeld -> ApiError::Busy,
    // not an opaque sync error.
    let err = racer_result.expect_err("the racer must lose the scope race");
    assert!(matches!(err, ApiError::Busy), "got {err:?}");
    assert_eq!(
        err.to_string(),
        "scope is busy: another sync is in progress; retry shortly"
    );
}

#[tokio::test]
async fn open_rejects_an_unusable_path() {
    let dir = tempfile::tempdir().unwrap();
    // A database file under a directory that does not exist cannot be created.
    let bad = dir.path().join("missing").join("engine.sqlite");
    let err = Engine::open(&bad).unwrap_err();
    assert!(matches!(err, ApiError::Store(_)), "got {err:?}");
}

#[tokio::test]
async fn searches_synced_mail() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();

    // Full-text over the indexed subject: "report" matches m1's "Quarterly report".
    let m1 = message("m1", "a", "Quarterly report").id.key().clone();
    let m2 = message("m2", "a", "Lunch plans").id.key().clone();
    let report = engine.search_mail(&account(), "report", 10).await.unwrap();
    assert_eq!(report.keys(), vec![m1.clone()]);
    assert!(report.coverage.is_complete());

    // A structured membership filter: both messages live in mailbox "a".
    let in_a = engine
        .search_mail(&account(), "mailbox:a", 10)
        .await
        .unwrap();
    let keys = in_a.keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&m1) && keys.contains(&m2));
}

#[tokio::test]
async fn searches_synced_calendar() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();

    // The event is a member of calendar "work"; the calendar-domain scopes are
    // enumerated and searched, not hard-coded.
    let evt = event("evt-1", "uid-1@h", "work").id.key().clone();
    let in_work = engine
        .search_calendar(&account(), "calendar:work", 10)
        .await
        .unwrap();
    assert_eq!(in_work.keys(), vec![evt]);
    assert!(in_work.coverage.is_complete());
}

#[tokio::test]
async fn search_rejects_a_malformed_query() {
    let engine = Engine::open_in_memory().unwrap();
    let mail_err = engine
        .search_mail(&account(), "from:", 10)
        .await
        .unwrap_err();
    assert!(matches!(mail_err, ApiError::Query(_)), "got {mail_err:?}");
    let cal_err = engine
        .search_calendar(&account(), "after:nope", 10)
        .await
        .unwrap_err();
    assert!(matches!(cal_err, ApiError::Query(_)), "got {cal_err:?}");
}

#[tokio::test]
async fn search_on_an_unsynced_account_is_empty() {
    let engine = Engine::open_in_memory().unwrap();
    // No sync has run, so the account has no scopes: an empty, vacuously complete answer.
    let results = engine.search_mail(&account(), "report", 10).await.unwrap();
    assert!(results.hits.is_empty());
    assert!(results.coverage.is_complete());
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

#[tokio::test]
async fn submit_mail_records_a_successful_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
    };
    let draft = draft("gen-1@test.local", "Quarterly report");

    let outcome = engine
        .submit_mail(&provider, &account(), &draft)
        .await
        .unwrap();
    assert_eq!(outcome.email_key, ProviderKey::new("sent-1").unwrap());
    assert_eq!(outcome.message_id, draft.message_id);
    // The durable op committed Succeeded, pollable by the returned id.
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn submit_mail_surfaces_a_failed_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
    };
    // A failed send surfaces as a sync error; the outbox records the op `Failed`
    // before returning (that recording is locked at the engine-sync layer).
    let err = engine
        .submit_mail(&provider, &account(), &draft("gen-2@test.local", "Lunch"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn pending_op_state_is_none_for_an_unknown_op() {
    let engine = Engine::open_in_memory().unwrap();
    assert_eq!(
        engine
            .pending_op_state(PendingOpId::new(999))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn sync_mail_streamed_reports_progress() {
    use std::sync::{Arc, Mutex};
    let engine = Engine::open_in_memory().unwrap();
    let seen: Arc<Mutex<Vec<SyncProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let seen = Arc::clone(&seen);
        // The blanket `ProgressSink for Fn(SyncProgress)` impl lets a closure be the sink.
        move |p: SyncProgress| seen.lock().unwrap().push(p)
    };

    let report = engine
        .sync_mail_streamed(&FakeProvider::new(), &account(), 0, &sink)
        .await
        .unwrap();
    assert_eq!(report.email.upserted, 2);

    // The fake returns both messages in one snapshot page whose total is known up
    // front, so exactly one progress event lands with fetched == total == 2.
    let progress = seen.lock().unwrap();
    assert_eq!(progress.len(), 1);
    assert_eq!(progress[0].fetched, 2);
    assert_eq!(progress[0].total, Some(2));
}

#[tokio::test]
async fn lists_synced_mailboxes_and_messages() {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_mail(&FakeProvider::new(), &account())
        .await
        .unwrap();

    // The two synced mailboxes, carrying their real names (not just keys).
    let mailboxes = engine.mailboxes(&account()).await.unwrap();
    let names: Vec<&str> = mailboxes.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(mailboxes.len(), 2);
    assert!(names.contains(&"Inbox") && names.contains(&"Archive"));

    // The two synced messages, carrying their real envelope subjects.
    let messages = engine.messages(&account()).await.unwrap();
    let subjects: Vec<&str> = messages
        .iter()
        .filter_map(|m| m.envelope.subject.as_deref())
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(subjects.contains(&"Quarterly report") && subjects.contains(&"Lunch plans"));

    // An account that never synced has neither.
    let other = AccountId::try_from("nobody").unwrap();
    assert!(engine.mailboxes(&other).await.unwrap().is_empty());
    assert!(engine.messages(&other).await.unwrap().is_empty());
}

#[tokio::test]
async fn lists_synced_calendars_and_events() {
    let engine = Engine::open_in_memory().unwrap();
    let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
    engine
        .sync_calendar(&FakeProvider::new(), &account(), horizon(), &zone)
        .await
        .unwrap();

    // The one synced calendar, carrying its real name (not just a key).
    let calendars = engine.calendars(&account()).await.unwrap();
    assert_eq!(calendars.len(), 1);
    assert_eq!(calendars[0].name, "Work");

    // The one synced event, carrying its real cross-system uid through the store.
    let events = engine.events(&account()).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, Uid::new("uid-1@h").unwrap());

    // An account that never synced has neither.
    let other = AccountId::try_from("nobody").unwrap();
    assert!(engine.calendars(&other).await.unwrap().is_empty());
    assert!(engine.events(&other).await.unwrap().is_empty());
}
