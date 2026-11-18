//! A transport that fetches a calendar only inside a date window: the window the engine opens
//! a cursor over and records, a range reached past it by reopening that cursor over the union,
//! and calendar maintenance that waits for a held scope rather than giving up.

use std::collections::BTreeMap;

use engine_core::{ids::DavCollectionId, time::UtcDateTime, version::CollectionTag};

use super::*;
use crate::{ScopeReach, reach_calendar, run_scope};

/// A windowed transport shaped like Graph's `calendarView/delta`: a snapshot lists the events
/// inside the window it was opened over, and its cursor names that window, so a delta reports
/// changes inside it and nowhere else. Built with `windowed: false` it stands for a transport that
/// fetches whole, and the engine still names a window it never reads.
#[derive(Default)]
struct FakeWindowed {
    windowed: bool,
    /// The tag the listing shows for the calendar, if it shows one.
    tag: Option<CollectionTag>,
    /// The calendar's events, by id: each with the instant it starts.
    events: Mutex<BTreeMap<String, Event>>,
    /// Events changed since the last fetch, by id.
    changed: Mutex<Vec<String>>,
    /// Every events request: whether it carried a cursor, and the window it named.
    requests: Mutex<Vec<(bool, Horizon)>>,
}

impl FakeWindowed {
    fn windowed() -> Self {
        Self {
            windowed: true,
            ..Self::default()
        }
    }

    /// Puts `id` on the server starting on `day` of 2026 (`month`, `day`), marked changed.
    fn put(&self, id: &str, month: u8, day: u8) {
        let event = Event::new(
            EventId::try_from(id).unwrap(),
            Uid::new(format!("{id}@h")).unwrap(),
            Memberships::of_one(cal()),
            CalendarDateTime::utc(LocalDateTime::new(2026, month, day, 9, 0, 0).unwrap()),
        );
        self.events.lock().unwrap().insert(id.to_owned(), event);
        self.changed.lock().unwrap().push(id.to_owned());
    }

    fn take_requests(&self) -> Vec<(bool, Horizon)> {
        core::mem::take(&mut *self.requests.lock().unwrap())
    }

    /// The events inside `window`, among `ids` (every event when `None`).
    fn inside(&self, window: Horizon, ids: Option<&[String]>) -> Vec<Event> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| ids.is_none_or(|ids| ids.contains(id)))
            .filter(|(_, event)| window.contains(day_of(event)))
            .map(|(_, event)| event.clone())
            .collect()
    }
}

#[async_trait::async_trait]
impl Provider for FakeWindowed {
    fn connection_info(&self) -> ConnectionInfo {
        let caps = Capabilities::none().with_calendars();
        ConnectionInfo::new(if self.windowed {
            caps.with_windowed_events()
        } else {
            caps
        })
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::DavCollectionList {
            account: account.clone(),
        }
    }

    fn event_scope(&self, account: &AccountId, calendar: &CalendarId) -> SyncScope {
        SyncScope::DavCollection {
            account: account.clone(),
            collection: DavCollectionId::new(calendar.key().clone()),
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<CalendarListing> {
        let listing = vec![Calendar::new(cal(), "home")];
        let present = listing.iter().map(|c| c.id.key().clone()).collect();
        let mut listed: CalendarListing = ScopeSync::new(
            SyncUpdate::snapshot(listing, present),
            SyncState::new("list"),
        )
        .into();
        listed.tags.extend(self.tag.clone().map(|tag| (cal(), tag)));
        Ok(listed)
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        _calendar: &CalendarId,
        cursor: Option<&SyncState>,
        window: Horizon,
    ) -> ProviderResult<ScopeSync<Event>> {
        self.requests
            .lock()
            .unwrap()
            .push((cursor.is_some(), window));
        let changed = core::mem::take(&mut *self.changed.lock().unwrap());
        // A transport that fetches whole reads no window at all.
        let window = if self.windowed {
            window
        } else {
            horizon("2000-01-01T00:00:00Z", "2100-01-01T00:00:00Z")
        };
        Ok(match cursor {
            None => {
                let events = self.inside(window, None);
                let present = events.iter().map(|e| e.id.key().clone()).collect();
                ScopeSync::new(SyncUpdate::snapshot(events, present), cursor_over(window))
            }
            // A delta reports changes inside the window its cursor was opened over, and keeps it.
            Some(cursor) => {
                let opened = window_of(cursor);
                let events = self.inside(opened, Some(&changed));
                ScopeSync::new(SyncUpdate::delta(events, vec![]), cursor_over(opened))
            }
        })
    }
}
impl CalendarWrites for FakeWindowed {}
impl engine_provider::MailboxWrites for FakeWindowed {}

fn cal() -> CalendarId {
    CalendarId::try_from("/cal/home/").unwrap()
}

fn horizon(from: &str, to: &str) -> Horizon {
    Horizon::new(from.parse().unwrap(), to.parse().unwrap()).unwrap()
}

/// The hour an event starts, as a window of its own.
fn day_of(event: &Event) -> Horizon {
    let CalendarDateTime::Zoned { local, .. } = event.start else {
        unreachable!("the fake stores UTC starts")
    };
    let at = UtcDateTime::new(local.year(), local.month(), local.day(), 9, 0, 0).unwrap();
    Horizon::new(
        at,
        UtcDateTime::new(local.year(), local.month(), local.day(), 10, 0, 0).unwrap(),
    )
    .unwrap()
}

fn cursor_over(window: Horizon) -> SyncState {
    SyncState::new(format!("{}|{}", window.start(), window.end()))
}

fn window_of(cursor: &SyncState) -> Horizon {
    let (start, end) = cursor.as_str().split_once('|').unwrap();
    Horizon::new(start.parse().unwrap(), end.parse().unwrap()).unwrap()
}

/// What the host shows: March 2026.
fn shown() -> Horizon {
    horizon("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z")
}

fn zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

fn scope() -> SyncScope {
    SyncScope::DavCollection {
        account: account(),
        collection: DavCollectionId::new(cal().key().clone()),
    }
}

async fn sync(provider: &FakeWindowed, store: &SqliteStore<ManualClock>) {
    let report = sync_calendar(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        shown(),
        &zone(),
    )
    .await
    .unwrap();
    assert_eq!(report.failures().count(), 0);
}

async fn reach(
    provider: &FakeWindowed,
    store: &SqliteStore<ManualClock>,
    range: Horizon,
) -> ScopeReach {
    let reach = reach_calendar(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        range,
    )
    .await
    .unwrap();
    assert!(reach.is_complete());
    let [scope] = <[_; 1]>::try_from(reach.scopes).unwrap();
    scope.result.unwrap()
}

async fn stored(store: &SqliteStore<ManualClock>) -> Vec<ProviderKey> {
    store.object_keys(&scope()).await.unwrap()
}

#[tokio::test]
async fn a_windowed_calendar_opens_over_the_window_the_host_shows_and_records_it() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-march", 3, 10);
    provider.put("evt-june", 6, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    sync(&provider, &store).await;

    assert_eq!(provider.take_requests(), vec![(false, shown())]);
    assert_eq!(stored(&store).await, vec![key("evt-march")]);
    assert_eq!(store.fetch_window(&scope()).await.unwrap(), Some(shown()));

    // A delta continues that window: the request names it, and the record stays.
    sync(&provider, &store).await;
    assert_eq!(provider.take_requests(), vec![(true, shown())]);
    assert_eq!(store.fetch_window(&scope()).await.unwrap(), Some(shown()));
}

#[tokio::test]
async fn a_range_inside_the_recorded_window_is_answered_without_a_request() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-march", 3, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    provider.take_requests();

    let week = horizon("2026-03-09T00:00:00Z", "2026-03-16T00:00:00Z");
    assert_eq!(reach(&provider, &store, week).await, ScopeReach::Covered);
    assert_eq!(provider.take_requests(), []);
}

#[tokio::test]
async fn a_range_past_the_window_reopens_the_cursor_over_the_union_once() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-march", 3, 10);
    provider.put("evt-june", 6, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    provider.take_requests();

    let june = horizon("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z");
    let reached = reach(&provider, &store, june).await;

    let union = horizon("2026-03-01T00:00:00Z", "2026-07-01T00:00:00Z");
    assert!(matches!(reached, ScopeReach::Fetched(_)));
    assert_eq!(
        provider.take_requests(),
        vec![(false, union)],
        "a fresh cursor over March to June, the gap included"
    );
    assert_eq!(
        stored(&store).await,
        vec![key("evt-june"), key("evt-march")]
    );
    assert_eq!(store.fetch_window(&scope()).await.unwrap(), Some(union));

    // Asked again, the range is covered: nothing is fetched twice.
    assert_eq!(reach(&provider, &store, june).await, ScopeReach::Covered);
    assert_eq!(provider.take_requests(), []);
}

#[tokio::test]
async fn an_event_reached_past_the_first_window_stays_current() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-march", 3, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    // Created in June after the first sync: the March cursor never reports it.
    provider.put("evt-june", 6, 10);
    sync(&provider, &store).await;
    assert_eq!(stored(&store).await, vec![key("evt-march")]);

    let june = horizon("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z");
    reach(&provider, &store, june).await;
    assert_eq!(
        stored(&store).await,
        vec![key("evt-june"), key("evt-march")]
    );

    // Moved within June afterwards: the reopened cursor reports it, as it would not have
    // reported a range merely listed beside the old one.
    provider.put("evt-june", 6, 20);
    sync(&provider, &store).await;
    let payload = store
        .object_payload(&scope(), &key("evt-june"))
        .await
        .unwrap()
        .unwrap();
    let event: Event = serde_json::from_value(payload).unwrap();
    assert_eq!(
        event.start,
        CalendarDateTime::utc(LocalDateTime::new(2026, 6, 20, 9, 0, 0).unwrap())
    );
}

#[tokio::test]
async fn a_calendar_fetched_whole_is_held_without_a_request() {
    let provider = FakeWindowed::default();
    provider.put("evt-june", 6, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    provider.take_requests();

    let june = horizon("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z");
    assert_eq!(reach(&provider, &store, june).await, ScopeReach::Covered);
    assert_eq!(provider.take_requests(), []);
    assert_eq!(store.fetch_window(&scope()).await.unwrap(), None);
    assert_eq!(
        stored(&store).await,
        vec![key("evt-june")],
        "fetched whole at the first sync"
    );
}

#[tokio::test]
async fn a_calendar_never_synced_is_not_reached() {
    let provider = FakeWindowed::windowed();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    // The list alone, as a pass whose event scopes never ran would leave it.
    run_scope(
        &store,
        &account(),
        &crate::calendar_scopes::CalendarScope(&provider),
        &LeaseRequest::new(worker(), Duration::from_mins(1)),
    )
    .await
    .unwrap();

    let reach = reach_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        shown(),
    )
    .await
    .unwrap();
    assert!(matches!(
        reach.scopes[0].result,
        Err(crate::SyncError::NoExpansionWindow)
    ));
    assert!(!reach.is_complete());
}

#[tokio::test]
async fn a_reach_waits_for_a_pass_holding_the_scope() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-june", 6, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    let held = store
        .claim_sync_scope(
            account(),
            &scope(),
            LeaseRequest::new(WorkerId::new("other"), Duration::from_mins(1)),
        )
        .await
        .unwrap();

    let june = horizon("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z");
    let reaching = reach(&provider, &store, june);
    tokio::pin!(reaching);
    tokio::select! {
        _ = &mut reaching => panic!("reached a scope another pass holds"),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    store.release_sync_scope(held.lease).await.unwrap();

    assert!(matches!(reaching.await, ScopeReach::Fetched(_)));
    assert_eq!(stored(&store).await, vec![key("evt-june")]);
}

#[tokio::test]
async fn an_expansion_waits_for_a_pass_holding_the_scope() {
    let provider = FakeWindowed::windowed();
    provider.put("evt-march", 3, 10);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    let held = store
        .claim_sync_scope(
            account(),
            &scope(),
            LeaseRequest::new(WorkerId::new("other"), Duration::from_mins(1)),
        )
        .await
        .unwrap();

    let wider = horizon("2026-02-01T00:00:00Z", "2026-05-01T00:00:00Z");
    let (account, zone) = (account(), zone());
    let expanding = expand_calendar_horizon(
        &store,
        &account,
        worker(),
        Duration::from_mins(1),
        wider,
        &zone,
    );
    tokio::pin!(expanding);
    tokio::select! {
        _ = &mut expanding => panic!("expanded a scope another pass holds"),
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    store.release_sync_scope(held.lease).await.unwrap();

    let expansion = expanding.await.unwrap();
    assert_eq!(expansion.occurrences, 1);
    assert_eq!(
        store
            .expansion_window(&scope())
            .await
            .unwrap()
            .unwrap()
            .horizon,
        wider
    );
}

mod race;
