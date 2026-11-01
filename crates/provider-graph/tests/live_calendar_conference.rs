//! Gated live check that an online meeting Exchange makes reads back as the calendar's own
//! conference.
//!
//! The event is created by direct HTTP with `isOnlineMeeting: true`, because asking the
//! server for a meeting is not something the adapter writes yet; it is the fixture. Reading it
//! back goes through the adapter's own sync, which is what a host sees. The account under test
//! is `GRAPH_ACCESS_TOKEN`. A calendar whose `allowedOnlineMeetingProviders` does not list
//! `teamsForBusiness` cannot make one, and the test says so and stops rather than failing.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_calendar_conference -- --nocapture
//! ```

mod common;

use common::*;
use engine_core::{
    calendar::{ConferenceOrigin, ConferenceService, EntryKind, NativeState},
    sync::SyncUpdate,
};
use engine_provider::Provider;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// An HTTP client on the engine's own TLS policy (a bare `reqwest::Client` has no rustls
/// crypto provider in this workspace).
fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

#[tokio::test]
async fn live_a_teams_meeting_reads_back_as_the_calendars_own_conference() {
    let Some(token) = token() else {
        eprintln!("skipping live_a_teams_meeting_reads_back...: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let calendar: serde_json::Value = http()
        .get(format!(
            "{GRAPH}/me/calendar?$select=allowedOnlineMeetingProviders"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /me/calendar")
        .json()
        .await
        .expect("calendar json");
    let providers = calendar["allowedOnlineMeetingProviders"].clone();
    eprintln!("allowedOnlineMeetingProviders: {providers}");
    if !providers
        .as_array()
        .is_some_and(|list| list.iter().any(|p| p == "teamsForBusiness"))
    {
        eprintln!("skipping: this calendar cannot make a Teams meeting");
        return;
    }

    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(14);
    let title = format!(
        "PIM live conference {}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let created: serde_json::Value = http()
        .post(format!("{GRAPH}/me/events"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "subject": title,
            "start": { "dateTime": format!("{date}T10:00:00"), "timeZone": "Europe/Amsterdam" },
            "end": { "dateTime": format!("{date}T10:30:00"), "timeZone": "Europe/Amsterdam" },
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "teamsForBusiness"
        }))
        .send()
        .await
        .expect("POST /me/events")
        .error_for_status()
        .expect("the server makes an online meeting")
        .json()
        .await
        .expect("created json");
    let id = created["id"].as_str().expect("an event id").to_owned();

    let provider = calendar_provider_around(&token, date);
    let outcome = read_back(&provider, &title).await;
    http()
        .delete(format!("{GRAPH}/me/events/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("DELETE the fixture event");

    let conferences = outcome.expect("the created event syncs");
    eprintln!("read back: {conferences:#?}");
    assert_eq!(conferences.len(), 1);
    let teams = &conferences[0];
    assert_eq!(teams.service, Some(ConferenceService::Teams));
    assert_eq!(
        teams.origin,
        ConferenceOrigin::Native {
            removable: false,
            state: NativeState::Ready
        }
    );
    let join = teams.join().expect("a join link");
    assert!(
        join.host().is_some_and(|host| host.starts_with("teams.")),
        "{join}"
    );
    assert_eq!(teams.entry_points[0].kind, EntryKind::Video);
}

/// The conferences of the event titled `title`, once the adapter's sync finds it.
async fn read_back(
    provider: &provider_graph::GraphCalendarProvider,
    title: &str,
) -> Option<Vec<engine_core::calendar::Conference>> {
    let calendars = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync;
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let calendar_id = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar")
        .id
        .clone();
    let events = provider
        .sync_events(&account(), &calendar_id, None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
        panic!("expected an event snapshot");
    };
    objects
        .into_iter()
        .find(|event| event.title == title)
        .map(|event| event.conferences)
}
