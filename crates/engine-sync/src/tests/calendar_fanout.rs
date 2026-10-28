//! One provider serving several calendars, each under an event scope of its own: the fan-out
//! [`sync_calendar`] owns, the collection-tag gate that skips an unchanged calendar, the
//! forgetting of a calendar that left the list, a failure that stays with its calendar, and a
//! reconcile that touches only the calendar written to.

use std::collections::BTreeMap;

use engine_core::{ids::DavCollectionId, version::CollectionTag};

use super::*;

/// One calendar on the fake server: its listing entry and tag, its events, and whether reading
/// its events fails.
#[derive(Clone)]
struct DavCalendar {
    listing: Calendar,
    tag: CollectionTag,
    events: Vec<Event>,
    fails: bool,
}

/// A CalDAV-shaped provider: one scope per calendar, each listing carrying a tag.
#[derive(Default)]
struct FakeDav {
    calendars: Mutex<BTreeMap<CalendarId, DavCalendar>>,
    /// Every calendar whose events were asked for, in order.
    fetched: Mutex<Vec<CalendarId>>,
}

/// An event on the fake server: its id, and the day in March 2026 it starts.
type Seeded<'a> = (&'a str, u8);

/// A calendar on the fake server: its name, its listing tag, and its events.
type Listed<'a> = (&'a str, &'a str, &'a [Seeded<'a>]);

impl FakeDav {
    fn with(calendars: &[Listed<'_>]) -> Self {
        let fake = Self::default();
        for (id, tag, events) in calendars {
            fake.put(id, tag, events);
        }
        fake
    }

    /// Lists `id` with `tag` and these events.
    fn put(&self, id: &str, tag: &str, events: &[Seeded<'_>]) {
        let calendar = cal(id);
        let listing = Calendar::new(calendar.clone(), id);
        let events = events
            .iter()
            .map(|(event, day)| {
                Event::new(
                    EventId::try_from(*event).unwrap(),
                    Uid::new(format!("{event}@h")).unwrap(),
                    Memberships::of_one(calendar.clone()),
                    march(*day),
                )
            })
            .collect();
        self.calendars.lock().unwrap().insert(
            calendar,
            DavCalendar {
                listing,
                tag: CollectionTag::new(tag),
                events,
                fails: false,
            },
        );
    }

    fn remove(&self, id: &str) {
        self.calendars.lock().unwrap().remove(&cal(id));
    }

    fn fail(&self, id: &str) {
        self.calendars
            .lock()
            .unwrap()
            .get_mut(&cal(id))
            .unwrap()
            .fails = true;
    }

    fn take_fetched(&self) -> Vec<CalendarId> {
        core::mem::take(&mut *self.fetched.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl Provider for FakeDav {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_calendars())
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
        let calendars = self.calendars.lock().unwrap();
        let listings: Vec<Calendar> = calendars
            .values()
            .map(|calendar| calendar.listing.clone())
            .collect();
        let tags = calendars
            .iter()
            .map(|(id, calendar)| (id.clone(), calendar.tag.clone()))
            .collect();
        let present = listings.iter().map(|c| c.id.key().clone()).collect();
        Ok(CalendarListing {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(listings, present),
                SyncState::new("list"),
            ),
            tags,
        })
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        calendar: &CalendarId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        self.fetched.lock().unwrap().push(calendar.clone());
        let held = self.calendars.lock().unwrap().get(calendar).cloned();
        let held = held.ok_or_else(|| ProviderError::permanent("no such calendar"))?;
        if held.fails {
            return Err(ProviderError::permanent("403 Forbidden"));
        }
        let present = held.events.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(held.events, present),
            SyncState::new(format!("cursor-{}", calendar.as_str())),
        ))
    }
}
impl CalendarWrites for FakeDav {}
impl engine_provider::MailboxWrites for FakeDav {}

fn cal(id: &str) -> CalendarId {
    CalendarId::try_from(format!("/cal/{id}/").as_str()).unwrap()
}

fn march(day: u8) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: LocalDateTime::new(2026, 3, day, 9, 0, 0).unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn horizon(from: &str, to: &str) -> Horizon {
    Horizon::new(from.parse().unwrap(), to.parse().unwrap()).unwrap()
}

fn zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

async fn sync(provider: &FakeDav, store: &SqliteStore<ManualClock>) -> crate::CalendarSyncReport {
    sync_calendar(
        provider,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        horizon("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z"),
        &zone(),
    )
    .await
    .unwrap()
}

async fn stored_keys(
    provider: &FakeDav,
    store: &SqliteStore<ManualClock>,
    id: &str,
) -> Vec<ProviderKey> {
    store
        .object_keys(&provider.event_scope(&account(), &cal(id)))
        .await
        .unwrap()
}

#[tokio::test]
async fn every_calendar_syncs_under_its_own_scope_from_one_provider() {
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("work", "w1", &[("evt-work", 3), ("evt-work-2", 4)]),
    ]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync(&provider, &store).await;

    assert_eq!(report.calendars.upserted, 2);
    assert_eq!(report.scopes.len(), 2, "one event scope per calendar");
    assert_eq!(report.failures().count(), 0);
    assert_eq!(report.events.applied.upserted, 3, "summed over both scopes");
    assert_eq!(
        stored_keys(&provider, &store, "home").await,
        vec![key("evt-home")]
    );
    assert_eq!(
        stored_keys(&provider, &store, "work").await,
        vec![key("evt-work"), key("evt-work-2")],
    );
}

#[tokio::test]
async fn a_calendar_whose_tag_has_not_moved_is_not_asked_again() {
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("work", "w1", &[("evt-work", 3)]),
    ]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    provider.take_fetched();

    let quiet = sync(&provider, &store).await;
    assert!(
        provider.take_fetched().is_empty(),
        "nothing changed, so nothing is fetched"
    );
    assert_eq!(quiet.unchanged(), 2);

    // One calendar changes: only it is fetched.
    provider.put("work", "w2", &[("evt-work", 3), ("evt-new", 9)]);
    let one = sync(&provider, &store).await;
    assert_eq!(provider.take_fetched(), vec![cal("work")]);
    assert_eq!(one.unchanged(), 1);
    assert_eq!(
        stored_keys(&provider, &store, "work").await,
        vec![key("evt-new"), key("evt-work")],
    );
}

#[tokio::test]
async fn a_calendar_that_leaves_the_list_takes_its_events_with_it() {
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("work", "w1", &[("evt-work", 3)]),
    ]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    provider.remove("work");
    let report = sync(&provider, &store).await;

    assert_eq!(report.forgotten, 1);
    assert_eq!(stored_keys(&provider, &store, "work").await, []);
    assert_eq!(
        stored_keys(&provider, &store, "home").await,
        vec![key("evt-home")]
    );

    // Coming back under the same href starts from nothing rather than an old cursor or tag.
    provider.put("work", "w1", &[("evt-work", 3)]);
    sync(&provider, &store).await;
    assert_eq!(
        stored_keys(&provider, &store, "work").await,
        vec![key("evt-work")]
    );
}

#[tokio::test]
async fn a_calendar_the_server_refuses_fails_alone() {
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("shared", "s1", &[("evt-shared", 3)]),
    ]);
    provider.fail("shared");
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync(&provider, &store).await;

    let failures: Vec<&SyncScope> = report.failures().map(|(scope, _)| scope).collect();
    assert_eq!(
        failures,
        vec![&provider.event_scope(&account(), &cal("shared"))]
    );
    assert_eq!(
        stored_keys(&provider, &store, "home").await,
        vec![key("evt-home")]
    );
    // A failed scope recorded no tag, so the next pass asks it again.
    provider.take_fetched();
    sync(&provider, &store).await;
    assert_eq!(provider.take_fetched(), vec![cal("shared")]);
}

#[tokio::test]
async fn a_write_to_a_calendar_whose_first_pass_failed_still_reconciles() {
    // The window is recorded before the first fetch, so the failed pass left one to expand
    // the written event over.
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("shared", "s1", &[("evt-shared", 3)]),
    ]);
    provider.fail("shared");
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;

    provider.put("shared", "s2", &[("evt-shared", 3), ("evt-written", 6)]);
    let report = reconcile_calendar_events(
        &provider,
        &store,
        &account(),
        &cal("shared"),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(report.applied.upserted, 2);
}

#[tokio::test]
async fn a_calendar_added_later_expands_over_the_window_the_account_already_has() {
    let provider = FakeDav::with(&[("home", "h1", &[("evt-home", 2)])]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    let year = horizon("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z");
    expand_calendar_horizon(
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        year,
        &zone(),
    )
    .await
    .unwrap();

    provider.put("later", "l1", &[("evt-later", 5)]);
    sync(&provider, &store).await;

    let window = store
        .expansion_window(&provider.event_scope(&account(), &cal("later")))
        .await
        .unwrap()
        .expect("seeded");
    assert_eq!(
        window.horizon, year,
        "not the narrower horizon the pass was handed"
    );
}

#[tokio::test]
async fn a_reconcile_reads_only_the_calendar_it_names() {
    let provider = FakeDav::with(&[
        ("home", "h1", &[("evt-home", 2)]),
        ("work", "w1", &[("evt-work", 3)]),
    ]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&provider, &store).await;
    provider.take_fetched();

    provider.put("work", "w2", &[("evt-work", 3), ("evt-written", 6)]);
    let report = reconcile_calendar_events(
        &provider,
        &store,
        &account(),
        &cal("work"),
        worker(),
        Duration::from_mins(1),
    )
    .await
    .unwrap();

    assert_eq!(provider.take_fetched(), vec![cal("work")]);
    assert_eq!(report.applied.upserted, 2);
    // The reconcile recorded no tag, so the next pass reads the calendar's new one.
    sync(&provider, &store).await;
    assert_eq!(provider.take_fetched(), vec![cal("work")]);
}

#[tokio::test]
async fn calendars_that_share_one_scope_sync_once() {
    // JMAP's event state is account-wide: every calendar answers the same scope.
    let provider = FakeMail::new(vec![], vec![]).with_calendar(
        vec![
            Calendar::new(CalendarId::try_from("a").unwrap(), "A"),
            Calendar::new(CalendarId::try_from("b").unwrap(), "B"),
        ],
        vec![],
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let report = sync_calendar(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        horizon("2026-03-01T00:00:00Z", "2026-04-01T00:00:00Z"),
        &zone(),
    )
    .await
    .unwrap();

    assert_eq!(report.scopes.len(), 1);
    assert_eq!(report.scopes[0].calendars.len(), 2);
}
