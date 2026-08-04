//! Read-your-writes for calendar writes through the facade (issue #65).
//!
//! The fake here is a **stateful server**, not a canned responder, because the bug this
//! locks down is only visible against one: it keeps its own copy of each event, enforces
//! the revision guard on every write (a stale `ETag` is a `Conflict`, as a CalDAV `412` or
//! a JMAP `stateMismatch` would be), **reserializes what it stores** (as Stalwart does —
//! `caldav.md`), and answers `sync_events` with a cursored delta.
//!
//! That is enough to prove the two things the issue is about:
//!
//! - after a write, the store holds the **server's** copy — not the bytes we sent, and not the
//!   pre-write copy;
//! - a host can therefore edit an event **twice**, re-reading it from the store in between, without
//!   the second edit being refused on a superseded guard.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use engine_api::{
    AccountId, ApiError, Engine, EventDeletion, EventDraft, EventPatch, Horizon, PatchTarget,
    Reconciled, TimeZoneId,
};
use engine_core::{
    calendar::{Calendar, Event, Participant, ParticipationStatus},
    error::FailureClass,
    ids::{CalendarId, EventId, ProviderKey, Uid},
    membership::Memberships,
    raw::RawIcal,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
    time::{CalendarDateTime, LocalDateTime},
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EventEdit, EventRsvp, EventWrite, EventWriteReceipt, Provider,
    ProviderError, ProviderResult, RsvpControls, ScopeSync, WriteGuard,
};

/// The account's own address — and deliberately **not** the one the invitation was sent to,
/// so a scenario that answers as `ALIAS_ADDRESS` proves the matched address travels rather
/// than being re-derived from the account (`rsvp.rs` → "Why the attendee address is carried").
const SELF_ADDRESS: &str = "me@test.local";

/// The address the seeded invitation was actually delivered to.
const ALIAS_ADDRESS: &str = "info@test.local";

/// What this fake advertises: a **server-scheduled** transport, the shape CalDAV's RFC 6638
/// auto-schedule has. The server emits the `REPLY` the moment it sees the status change, so
/// there is nowhere to put a note and no way to keep the organizer out of it — and the guard
/// is enforced, as a `412` would be. Declared once and used to both advertise and enforce, so
/// the fake cannot drift from what it claims.
///
/// JMAP looks server-scheduled but is **not** one of these: it schedules only when the
/// request asks (`sendSchedulingMessages`), so it honours the quiet toggle. CalDAV is the
/// only transport this fake still describes.
const SERVER_SCHEDULED: RsvpControls = RsvpControls {
    comment: false,
    suppress_notification: false,
    guard: WriteGuard::Enforced,
};

#[path = "calendar_writes/scenarios.rs"]
mod scenarios;

/// One event as the server holds it, with the revision it is guarded by and the pass it
/// last changed on (the cursor is that pass number).
#[derive(Clone)]
struct Stored {
    event: Event,
    etag: ETag,
    version: u64,
}

#[derive(Default)]
struct ServerState {
    version: u64,
    events: BTreeMap<String, Stored>,
    destroyed: Vec<(u64, ProviderKey)>,
}

/// A calendar server that keeps state: it enforces the guard, stamps its own revisions,
/// stores its *own* serialization of what it is sent, and reports changes as a delta.
#[derive(Clone)]
struct CalendarServer(Arc<Mutex<ServerState>>);

impl CalendarServer {
    /// A server already holding `event` at revision `"srv-1"` — an event a first sync would
    /// bring down.
    fn holding(event: Event) -> Self {
        let mut state = ServerState {
            version: 1,
            ..ServerState::default()
        };
        state.events.insert(
            event.id.as_str().to_owned(),
            Stored {
                event: server_copy(event, "srv-1"),
                etag: ETag::new("\"srv-1\""),
                version: 1,
            },
        );
        Self(Arc::new(Mutex::new(state)))
    }

    /// Refuses a write whose guard is not the revision the server currently holds — a
    /// CalDAV `412`, a JMAP `stateMismatch`.
    fn check_guard(
        state: &ServerState,
        event: &EventId,
        guard: Option<&RevisionTokens>,
    ) -> ProviderResult<()> {
        let Some(stored) = state.events.get(event.as_str()) else {
            return Err(ProviderError::conflict("no such event"));
        };
        match guard.and_then(|tokens| tokens.etag.as_ref()) {
            Some(etag) if *etag != stored.etag => Err(ProviderError::conflict(
                "etag precondition failed: the caller's copy is stale",
            )),
            _ => Ok(()),
        }
    }

    /// Commits a new revision of `event`, stamping the server's own `ETag` and its own
    /// serialization.
    fn commit(state: &mut ServerState, mut event: Event) -> EventWriteReceipt {
        state.version += 1;
        let version = state.version;
        let revision = format!("srv-{version}");
        event = server_copy(event, &revision);
        let etag = ETag::new(format!("\"{revision}\""));
        let id = event.id.clone();
        let uid = event.uid.clone();
        state.events.insert(
            id.as_str().to_owned(),
            Stored {
                event,
                etag: etag.clone(),
                version,
            },
        );
        EventWriteReceipt::new(id, uid, RevisionTokens::from_etag(etag))
    }
}

/// What the server *stores*, which is never byte-identical to what it was sent: it keeps
/// the properties but re-serializes the document (Stalwart re-folds content lines and
/// reorders `RRULE` parts). The marker stands in for that, so a test can tell the store's
/// copy came from the server rather than from the bytes the write sent.
fn server_copy(mut event: Event, revision: &str) -> Event {
    event.raw_ical = Some(RawIcal::new(format!(
        "BEGIN:VCALENDAR\r\nX-SERVER-SERIALIZED:{revision}\r\nEND:VCALENDAR"
    )));
    event.revisions = RevisionTokens::from_etag(ETag::new(format!("\"{revision}\"")));
    event
}

#[async_trait::async_trait]
impl Provider for CalendarServer {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(
            Capabilities::none()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Enforced)
                .with_calendar_rsvp(SERVER_SCHEDULED),
        )
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

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        let calendars = vec![Calendar::new(CalendarId::try_from("work").unwrap(), "Work")];
        let present = calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(calendars, present),
            SyncState::new("cal-1"),
        ))
    }

    /// A snapshot with no cursor, a delta with one: everything changed since it, plus what
    /// was destroyed since it. The cursor is the server's version counter.
    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        let state = self.0.lock().unwrap();
        let next = SyncState::new(state.version.to_string());
        let Some(since) = cursor.map(|c| c.as_str().parse::<u64>().unwrap()) else {
            let objects: Vec<Event> = state.events.values().map(|s| s.event.clone()).collect();
            let present = objects.iter().map(|e| e.id.key().clone()).collect();
            return Ok(ScopeSync::new(SyncUpdate::snapshot(objects, present), next));
        };
        let changed: Vec<Event> = state
            .events
            .values()
            .filter(|s| s.version > since)
            .map(|s| s.event.clone())
            .collect();
        let removed: Vec<ProviderKey> = state
            .destroyed
            .iter()
            .filter(|(version, _)| *version > since)
            .map(|(_, key)| key.clone())
            .collect();
        Ok(ScopeSync::new(SyncUpdate::delta(changed, removed), next))
    }

    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        let mut state = self.0.lock().unwrap();
        let id = EventId::try_from(format!("/cal/{}.ics", draft.uid.as_str()).as_str()).unwrap();
        if state.events.contains_key(id.as_str()) {
            return Err(ProviderError::conflict("an event already exists there"));
        }
        let mut event = Event::new(
            id,
            draft.uid.clone(),
            Memberships::of_one(draft.calendar.clone()),
            draft.start.clone(),
        );
        event.title.clone_from(&draft.summary);
        Ok(CalendarServer::commit(&mut state, event))
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        let mut state = self.0.lock().unwrap();
        CalendarServer::check_guard(&state, &edit.event, Some(&base.revisions))?;
        // The surgery a real adapter does in its own protocol, reduced to what these tests
        // exercise: retitle, and move.
        let mut event = state.events[edit.event.as_str()].event.clone();
        if let Some(summary) = edit.patch.summary_edit() {
            summary.clone_into(&mut event.title);
        }
        if let Some(start) = edit.patch.start_edit() {
            event.start = start.clone();
        }
        Ok(CalendarServer::commit(&mut state, event))
    }

    async fn put_event(
        &self,
        _account: &AccountId,
        write: &EventWrite,
    ) -> ProviderResult<EventWriteReceipt> {
        let mut state = self.0.lock().unwrap();
        CalendarServer::check_guard(&state, &write.event, write.guard.as_ref())?;
        let event = state.events[write.event.as_str()].event.clone();
        Ok(CalendarServer::commit(&mut state, event))
    }

    /// Answers as a **scheduling** server does: it refuses a control it does not honour,
    /// enforces the guard, moves exactly one participant's status — the one whose address
    /// matches — and refuses outright when no participant carries that address.
    ///
    /// That last case is the one worth having a fake for. A real server answers it with a
    /// `403`/`invalidPatch` rather than inventing an attendee, and an adapter that "helpfully"
    /// appended one would put the user on a meeting the organizer never invited them to.
    async fn rsvp_event(
        &self,
        _account: &AccountId,
        _base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        SERVER_SCHEDULED.accept(rsvp)?;
        let mut state = self.0.lock().unwrap();
        CalendarServer::check_guard(&state, &rsvp.event, rsvp.guard.as_ref())?;
        let mut event = state.events[rsvp.event.as_str()].event.clone();
        let me = event
            .participants
            .iter_mut()
            .find(|p| p.email.as_deref() == Some(rsvp.attendee.as_str()))
            .ok_or_else(|| {
                ProviderError::invalid_state(
                    "no ATTENDEE at that address — the answer names someone this meeting \
                     does not have",
                )
            })?;
        me.participation_status = rsvp.response.status();
        Ok(CalendarServer::commit(&mut state, event))
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        let mut state = self.0.lock().unwrap();
        CalendarServer::check_guard(&state, &deletion.event, deletion.guard.as_ref())?;
        state.version += 1;
        let version = state.version;
        state.events.remove(deletion.event.as_str());
        state
            .destroyed
            .push((version, deletion.event.key().clone()));
        Ok(())
    }
}

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn host_zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

fn horizon() -> Horizon {
    Horizon::new(
        "2026-01-01T00:00:00Z".parse().unwrap(),
        "2026-12-31T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

/// The one-day window the seeded event falls in.
fn march_first() -> Horizon {
    Horizon::new(
        "2026-03-01T00:00:00Z".parse().unwrap(),
        "2026-03-02T00:00:00Z".parse().unwrap(),
    )
    .unwrap()
}

fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(2026, 3, 1, hour, 0, 0).unwrap(),
        zone: host_zone(),
    }
}

fn seeded_event() -> Event {
    let mut event = Event::new(
        EventId::try_from("/cal/evt-1.ics").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        Memberships::of_one(CalendarId::try_from("work").unwrap()),
        at(9),
    );
    "Standup".clone_into(&mut event.title);
    event.duration = "PT30M".parse().unwrap();
    // An invitation, not a private appointment: the organizer, and us — invited at the
    // **alias** the invitation was delivered to, never at the account's own address. An RSVP
    // that answered as `SELF_ADDRESS` would find no `ATTENDEE` here, which is exactly the
    // failure the matched-address rule exists to prevent.
    event.participants = vec![
        Participant::attendee("organizer@test.local"),
        Participant::attendee(ALIAS_ADDRESS),
    ];
    event
}

/// Syncs the server into a fresh engine and hands back the stored event — what a host
/// reads before it edits.
async fn synced(server: &CalendarServer) -> (Engine, Event) {
    let engine = Engine::open_in_memory().unwrap();
    engine
        .sync_calendar(server, &account(), horizon(), &host_zone())
        .await
        .unwrap();
    let stored = engine.events(&account()).await.unwrap().remove(0);
    (engine, stored)
}

/// A provider whose event fetch parks until it is released, so a test can hold the event
/// scope's lease while another call tries to reconcile.
struct BlockingSync {
    inner: CalendarServer,
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Provider for BlockingSync {
    fn connection_info(&self) -> ConnectionInfo {
        self.inner.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.inner.email_scope(account)
    }

    async fn sync_events(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        if let Some(started) = self.started.lock().unwrap().take() {
            started.send(()).unwrap();
        }
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            // The lease is held across this await — exactly the window a real concurrent
            // sync leaves open.
            let _ = release.await;
        }
        self.inner.sync_events(account, cursor).await
    }
}

/// A provider whose writes land but whose event fetch is broken, so the post-write
/// reconcile fails on its own rather than on a held lease.
struct UnreadableEvents(CalendarServer);

#[async_trait::async_trait]
impl Provider for UnreadableEvents {
    fn connection_info(&self) -> ConnectionInfo {
        self.0.connection_info()
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        self.0.mailbox_scope(account)
    }

    fn email_scope(&self, account: &AccountId) -> SyncScope {
        self.0.email_scope(account)
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        Err(ProviderError::retryable("the event fetch is down"))
    }

    async fn patch_event(
        &self,
        account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        self.0.patch_event(account, base, edit).await
    }
}
