//! Offline tests for [`GraphCalendarProvider`]: scopes, capabilities, the drained
//! calendar/event syncs (fake transport + a real-HTTP replay server), and the
//! calendar-binding guard on create.

use engine_core::{ids::AccountId, sync::SyncUpdate, time::CalendarDate};
use engine_provider::{EventDraft, Provider, WriteGuard};

use super::*;
use crate::test_support::{fake_client, json, replay_server, retry, tls};

const CALENDARS: &str = include_str!("../tests/fixtures/calendar/calendars.json");
const DELTA: &str = include_str!("../tests/fixtures/calendar/events_delta.json");

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

fn calendar() -> CalendarId {
    CalendarId::try_from("cal-1").unwrap()
}

fn window() -> CalendarWindow {
    CalendarWindow::new(
        CalendarDate::new(2026, 8, 1).unwrap(),
        CalendarDate::new(2026, 11, 1).unwrap(),
    )
}

fn routes() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("/calendars?$top", json(CALENDARS)),
        ("calendarView/delta", json(DELTA)),
    ]
}

fn provider(client: GraphClient) -> GraphCalendarProvider {
    GraphCalendarProvider::new(
        client,
        calendar(),
        window(),
        engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    )
}

#[test]
fn debug_names_the_binding_without_leaking_the_token() {
    let provider = GraphCalendarProvider::new(
        GraphClient::connect("super-secret", tls(), retry()).unwrap(),
        calendar(),
        window(),
        engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    );
    let debug = format!("{provider:?}");
    assert!(debug.contains("cal-1") && debug.contains("Europe/Amsterdam"));
    assert!(!debug.contains("super-secret"));
}

#[test]
fn exposes_graph_calendar_scopes_and_write_capabilities() {
    let provider = provider(fake_client(vec![]));
    assert_eq!(
        provider.calendar_scope(&account()),
        SyncScope::GraphCalendarList { account: account() }
    );
    assert_eq!(
        provider.event_scope(&account()),
        SyncScope::GraphCalendar {
            account: account(),
            calendar: calendar(),
        }
    );
    let caps = provider.connection_info().capabilities;
    assert!(caps.calendars() && caps.calendar_writes());
    // Graph enforces the lost-update guard (If-Match), unlike JMAP.
    assert_eq!(caps.calendar_write_guard(), Some(WriteGuard::Enforced));
    assert!(!caps.mail());
}

#[tokio::test]
async fn syncs_the_calendar_list_and_an_event_snapshot() {
    let provider = provider(fake_client(routes()));

    let calendars = provider.sync_calendars(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0].name, "Calendar");
    assert_eq!(objects[1].name, "Extra calendar test");

    let events = provider.sync_events(&account(), None).await.unwrap();
    assert!(events.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    // Master + 2 singles kept; occurrences + exception dropped.
    assert_eq!(objects.len(), 3);
    assert_eq!(objects.iter().filter(|e| e.is_recurring()).count(), 1);
}

#[tokio::test]
async fn end_to_end_against_a_fixture_replay_server() {
    // Drive the whole stack (reqwest transport + @odata rebasing + drain) over real HTTP.
    let base = replay_server(routes());
    let client = GraphClient::with_base("t", base, tls(), retry()).unwrap();
    let provider = provider(client);
    let events = provider.sync_events(&account(), None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected a snapshot");
    };
    assert_eq!(objects.len(), 3);
}

#[tokio::test]
async fn sync_events_from_a_cursor_is_a_delta() {
    use crate::test_support::fake_client_fallible;
    let cursor = engine_core::sync::SyncState::new("https://graph.test/me/cursor-token");
    let client = fake_client_fallible(vec![("cursor-token", Ok(json(DELTA)))]);
    let events = provider(client)
        .sync_events(&account(), Some(&cursor))
        .await
        .unwrap();
    assert!(!events.is_snapshot());
    let SyncUpdate::Delta { changed, .. } = &events.update else {
        panic!("expected a delta");
    };
    assert_eq!(changed.len(), 3);
}

#[tokio::test]
async fn sync_events_restarts_as_a_snapshot_when_the_deltalink_expired() {
    use crate::test_support::fake_client_fallible;
    // A stored deltaLink Graph has aged out (`410`) drops the cursor and restarts the
    // pass as a full snapshot, so the scope is never wedged.
    let cursor = engine_core::sync::SyncState::new("https://graph.test/me/expired-token");
    let client = fake_client_fallible(vec![
        ("expired-token", Err((410, serde_json::json!({})))),
        ("calendarView/delta", Ok(json(DELTA))),
    ]);
    let events = provider(client)
        .sync_events(&account(), Some(&cursor))
        .await
        .unwrap();
    assert!(
        events.is_snapshot(),
        "the expired delta restarts as a snapshot"
    );
}

#[tokio::test]
async fn create_patch_delete_dispatch_to_the_writer() {
    use engine_core::{
        calendar::Event,
        ids::{EventId, Uid},
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId},
        version::{ETag, RevisionTokens},
    };
    use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget};

    use crate::test_support::fake_client_fallible;

    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let stamp = "2026-07-18T10:00:00Z".parse().unwrap();
    let created =
        serde_json::json!({ "id": "new-id", "iCalUId": "new-uid", "@odata.etag": "W/\"v1\"" });
    let client = fake_client_fallible(vec![
        (
            "/events/evt-1",
            Ok(serde_json::json!({ "id": "evt-1", "iCalUId": "u", "@odata.etag": "W/\"v2\"" })),
        ),
        ("/events", Ok(created)),
    ]);
    let provider = provider(client);

    // Create in the bound calendar.
    let draft = EventDraft::new(
        calendar(),
        Uid::new("d@test.local").unwrap(),
        "probe",
        zoned("2026-09-01T09:00:00"),
        zoned("2026-09-01T09:30:00"),
        stamp,
    );
    let receipt = provider.create_event(&account(), &draft).await.unwrap();
    assert_eq!(receipt.event.as_str(), "new-id");

    // Patch + delete a synced event.
    let mut base = Event::new(
        EventId::try_from("evt-1").unwrap(),
        Uid::new("u").unwrap(),
        Memberships::of_one(calendar()),
        zoned("2026-09-01T09:00:00"),
    );
    base.revisions = RevisionTokens::from_etag(ETag::new("W/\"v1\""));
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp).summary("x"),
    );
    assert!(provider.patch_event(&account(), &base, &edit).await.is_ok());
    assert!(
        provider
            .delete_event(&account(), None, &EventDeletion::of(&base))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn create_refuses_a_draft_targeting_another_calendar() {
    use engine_core::time::{CalendarDateTime, LocalDateTime, TimeZoneId};
    let zoned = |s: &str| CalendarDateTime::Zoned {
        local: s.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    };
    let provider = provider(fake_client(vec![]));
    let draft = EventDraft::new(
        CalendarId::try_from("other-cal").unwrap(),
        engine_core::ids::Uid::new("u@test.local").unwrap(),
        "elsewhere",
        zoned("2026-08-03T09:00:00"),
        zoned("2026-08-03T09:30:00"),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    );
    assert!(provider.create_event(&account(), &draft).await.is_err());
}
