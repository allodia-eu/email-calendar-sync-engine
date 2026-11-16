//! A windowed calendar through the facade: a range past the first sync's window fetched by
//! `reach_calendar`, and `expand_horizon` waiting for a sync that holds the calendar instead of
//! answering `Busy`.

use std::sync::{Arc, Mutex};

use engine_api::{ApiError, Engine, ScopeReach, TimeZoneId};

use super::*;

/// A transport shaped like Graph and Google: it advertises a windowed event fetch, lists only the
/// events inside the window a snapshot names, and can park inside a fetch (holding the
/// calendar's lease) until the test lets it go.
struct Windowed {
    events: Vec<Event>,
    /// Every window an events request named, and whether it carried a cursor.
    requests: Mutex<Vec<(bool, Horizon)>>,
    on_fetch: Mutex<Option<oneshot::Sender<()>>>,
    until_release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Windowed {
    fn new(events: Vec<Event>) -> Self {
        Self {
            events,
            requests: Mutex::default(),
            on_fetch: Mutex::default(),
            until_release: Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Windowed {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_calendars().with_windowed_events())
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<CalendarListing> {
        let listed = vec![calendar("cal-1", "Home")];
        let present = listed.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(listed, present),
            SyncState::new("cal-1"),
        )
        .into())
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
        if let Some(tx) = self.on_fetch.lock().unwrap().take() {
            let _ = tx.send(());
        }
        let release = self.until_release.lock().unwrap().take();
        if let Some(rx) = release {
            let _ = rx.await;
        }
        if cursor.is_some() {
            return Ok(ScopeSync::new(
                SyncUpdate::delta(Vec::new(), Vec::new()),
                SyncState::new("evt-2"),
            ));
        }
        let inside: Vec<Event> = self
            .events
            .iter()
            .filter(|event| starts_inside(event, window))
            .cloned()
            .collect();
        let present = inside.iter().map(|e| e.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(inside, present),
            SyncState::new("evt-1"),
        ))
    }
}
impl engine_provider::MailboxWrites for Windowed {}
impl CalendarWrites for Windowed {}

fn starts_inside(event: &Event, window: Horizon) -> bool {
    let CalendarDateTime::Zoned { local, .. } = event.start else {
        unreachable!("the fixtures start in UTC")
    };
    let at = format!(
        "{:04}-{:02}-{:02}T{:02}:00:00Z",
        local.year(),
        local.month(),
        local.day(),
        local.hour()
    );
    window.overlaps(at.parse().unwrap(), at.parse().unwrap())
}

fn one_off(id: &str, month: u8, day: u8) -> Event {
    let mut event = event(id, &format!("{id}@h"), "cal-1");
    event.start = CalendarDateTime::utc(LocalDateTime::new(2026, month, day, 9, 0, 0).unwrap());
    event
}

fn span(from: &str, to: &str) -> Horizon {
    Horizon::new(from.parse().unwrap(), to.parse().unwrap()).unwrap()
}

fn zone() -> TimeZoneId {
    TimeZoneId::iana("Europe/Amsterdam").unwrap()
}

/// The host's first horizon: the first quarter of 2026.
fn first_quarter() -> Horizon {
    span("2026-01-01T00:00:00Z", "2026-04-01T00:00:00Z")
}

async fn occurrence_events(engine: &Engine, range: Horizon) -> Vec<String> {
    engine
        .occurrences_in(&account(), range)
        .await
        .unwrap()
        .iter()
        .map(|row| row.event.as_str().to_owned())
        .collect()
}

/// A page past the first sync's window: expanding alone shows nothing there, because the
/// one-off event was never fetched; reaching the range fetches it once, and asking again
/// fetches nothing.
#[tokio::test]
async fn reaching_a_range_past_the_first_window_fetches_its_events_once() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = Windowed::new(vec![one_off("evt-feb", 2, 3), one_off("evt-sep", 9, 15)]);
    engine
        .sync_calendar(&provider, &account(), first_quarter(), &zone())
        .await
        .unwrap();
    let september = span("2026-09-01T00:00:00Z", "2026-10-01T00:00:00Z");
    let shown = first_quarter().union(september);

    engine
        .expand_horizon(&account(), shown, &zone())
        .await
        .unwrap();
    assert_eq!(
        occurrence_events(&engine, september).await,
        Vec::<String>::new(),
        "expanding re-derives only what was fetched"
    );

    provider.requests.lock().unwrap().clear();
    let reach = engine
        .reach_calendar(&provider, &account(), september)
        .await
        .unwrap();
    assert!(reach.is_complete());
    assert!(matches!(reach.scopes[0].result, Ok(ScopeReach::Fetched(_))));
    assert_eq!(
        *provider.requests.lock().unwrap(),
        vec![(false, shown)],
        "one fresh snapshot over the first window and the range together",
    );
    assert_eq!(occurrence_events(&engine, september).await, vec!["evt-sep"]);
    assert_eq!(
        occurrence_events(&engine, first_quarter()).await,
        vec!["evt-feb"]
    );

    provider.requests.lock().unwrap().clear();
    let again = engine
        .reach_calendar(&provider, &account(), september)
        .await
        .unwrap();
    assert!(matches!(again.scopes[0].result, Ok(ScopeReach::Held)));
    assert_eq!(*provider.requests.lock().unwrap(), []);
}

/// A sync holding the calendar makes `expand_horizon` wait for it, then expand, rather than
/// answer `Busy` and leave the host to guess when to ask again.
#[tokio::test]
async fn expanding_waits_for_a_sync_holding_the_calendar() {
    let engine = Arc::new(Engine::open_in_memory().unwrap());
    let provider = Arc::new(Windowed::new(vec![one_off("evt-feb", 2, 3)]));
    engine
        .sync_calendar(&*provider, &account(), first_quarter(), &zone())
        .await
        .unwrap();

    let (fetching, fetched) = oneshot::channel();
    let (release, until_release) = oneshot::channel();
    *provider.on_fetch.lock().unwrap() = Some(fetching);
    *provider.until_release.lock().unwrap() = Some(until_release);
    let sync = tokio::spawn({
        let (engine, provider) = (Arc::clone(&engine), Arc::clone(&provider));
        async move {
            engine
                .reconcile_calendar_events(&*provider, &account(), &calendar("cal-1", "Home").id)
                .await
        }
    });
    fetched.await.unwrap();

    let wider = span("2025-12-01T00:00:00Z", "2026-05-01T00:00:00Z");
    let (account, zone) = (account(), zone());
    let expanding = engine.expand_horizon(&account, wider, &zone);
    tokio::pin!(expanding);
    tokio::select! {
        done = &mut expanding => panic!("expanded under a held calendar: {done:?}"),
        () = tokio::time::sleep(core::time::Duration::from_millis(100)) => {}
    }
    release.send(()).unwrap();

    let expansion = expanding.await;
    assert!(
        !matches!(expansion, Err(ApiError::Busy)),
        "a held calendar is waited for"
    );
    assert_eq!(expansion.unwrap().occurrences, 1);
    sync.await.unwrap().unwrap();
}
