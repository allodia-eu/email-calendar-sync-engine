//! Gated live checks that an online meeting Exchange makes reads back as the calendar's own
//! conference, without Exchange's stand-in location, that a meeting only written into the
//! location and body is detected there, that editing a meeting's notes keeps the meeting, and
//! that each calendar says whether its server makes Teams meetings.
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
    calendar::{
        ConferenceOrigin, ConferenceService, ConferenceUri, DetectedIn, EntryKind, Event,
        NativeState,
    },
    error::FailureClass,
    sync::SyncUpdate,
};
use engine_provider::{CalendarWrites, EventEdit, EventPatch, PatchTarget, Provider};

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

    let event = outcome.expect("the created event syncs");
    let conferences = &event.conferences;
    eprintln!(
        "read back: {conferences:#?}, locations {:?}",
        event.locations
    );
    // The body's join link is the same meeting, and Exchange's stand-in location is none.
    assert_eq!(event.locations, []);
    // The body is all Teams block, inner section rules included, so nothing is the person's.
    assert_eq!(event.notes(|_| true), None, "{:?}", event.description);
    assert_eq!(conferences.len(), 1);
    let teams = &conferences[0];
    assert_eq!(teams.service, Some(ConferenceService::Teams));
    assert_eq!(
        teams.origin,
        ConferenceOrigin::Native {
            removable: true,
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

#[tokio::test]
async fn live_a_meeting_only_in_the_body_and_location_is_detected() {
    let Some(token) = token() else {
        eprintln!("skipping live_a_meeting_only_in_the_body...: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(15);
    let title = format!(
        "PIM live detected conference {}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let created: serde_json::Value = http()
        .post(format!("{GRAPH}/me/events"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "subject": title,
            "start": { "dateTime": format!("{date}T10:00:00"), "timeZone": "Europe/Amsterdam" },
            "end": { "dateTime": format!("{date}T10:30:00"), "timeZone": "Europe/Amsterdam" },
            "location": { "displayName": "https://video.example.com/room-12" },
            "body": {
                "contentType": "html",
                "content": "<p>Agenda first.</p><p><a href=\"https://zoom.us/j/123456789?pwd=abc&amp;from=addon\">Join Zoom Meeting</a></p>"
            }
        }))
        .send()
        .await
        .expect("POST /me/events")
        .error_for_status()
        .expect("the server stores the event")
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

    let event = outcome.expect("the created event syncs");
    eprintln!("read back: {:#?}", event.conferences);
    let found: Vec<_> = event
        .conferences
        .iter()
        .map(|c| (c.join().map(ConferenceUri::as_str), c.origin))
        .collect();
    assert_eq!(
        found,
        [
            (
                Some("https://video.example.com/room-12"),
                ConferenceOrigin::Detected {
                    place: DetectedIn::Location
                }
            ),
            (
                Some("https://zoom.us/j/123456789?pwd=abc&from=addon"),
                ConferenceOrigin::Detected {
                    place: DetectedIn::Description
                }
            ),
        ]
    );
    assert_eq!(event.conferences[1].service, Some(ConferenceService::Zoom));
}

#[tokio::test]
async fn live_a_notes_edit_keeps_the_teams_meeting_and_the_whole_notes() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_notes_edit_keeps_the_teams_meeting...: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(16);
    let title = format!(
        "PIM live notes edit {}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    // Longer than the 255 characters of `bodyPreview`, so a description read from the preview
    // would lose its last sentence.
    let notes = "<p>Agenda first: the figures, then the forecast.</p><p>Please read the \
        <a href=\"https://example.com/q3-report\">Q3 report</a> before we meet, and bring any \
        questions about the regional split, the hiring plan and the budget for next year. We \
        keep the last ten minutes for decisions and owners.</p>";
    let created: serde_json::Value = http()
        .post(format!("{GRAPH}/me/events"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "subject": title,
            "start": { "dateTime": format!("{date}T11:00:00"), "timeZone": "Europe/Amsterdam" },
            "end": { "dateTime": format!("{date}T11:30:00"), "timeZone": "Europe/Amsterdam" },
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "teamsForBusiness",
            "body": { "contentType": "html", "content": notes }
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

    // A calendar that cannot make a Teams meeting answers with a plain event.
    let outcome = if created["isOnlineMeeting"] == true {
        let provider = calendar_provider_around(&token, date);
        Some(edit_notes(&provider, &token, &title, &id).await)
    } else {
        None
    };
    http()
        .delete(format!("{GRAPH}/me/events/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("DELETE the fixture event");
    let Some(outcome) = outcome else {
        eprintln!("skipping: this calendar cannot make a Teams meeting");
        return;
    };

    let (read, after) = outcome.unwrap();
    assert!(
        read.contains("decisions and owners."),
        "the description is the whole body: {read}"
    );
    assert_eq!(after["isOnlineMeeting"], true, "{after:#}");
    assert!(after["onlineMeeting"]["joinUrl"].is_string(), "{after:#}");
    let body = after["body"]["content"].as_str().unwrap_or_default();
    assert!(body.contains("Agenda first, edited"), "{body}");
    assert!(body.contains("decisions and owners."), "{body}");
    assert!(body.contains("https://example.com/q3-report"), "{body}");
}

/// Edits the notes of the event titled `title` through the adapter, keeping the rest of its
/// description, and returns the description the sync read and the event as Graph then holds
/// it. Exchange keeps changing an event for a few seconds while it sets up a Teams meeting,
/// so a revision read just after the create can be refused as stale (`412`); like the outbox,
/// the edit is then rebuilt on a fresh read. Nothing here panics, so the caller's clean-up
/// always runs.
async fn edit_notes(
    provider: &provider_graph::GraphCalendarProvider,
    token: &str,
    title: &str,
    id: &str,
) -> Result<(String, serde_json::Value), String> {
    for _ in 0..5 {
        let base = read_back(provider, title)
            .await
            .ok_or("the created event syncs")?;
        let read = base
            .description
            .clone()
            .ok_or("the event has a description")?;
        let edit =
            EventEdit::new(
                &base,
                PatchTarget::Series,
                EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap())
                    .description(read.replacen("Agenda first", "Agenda first, edited", 1)),
            );
        match provider.patch_event(&account(), &base, &edit).await {
            Ok(_) => {}
            Err(err) if err.class() == FailureClass::Conflict => {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
            Err(err) => return Err(format!("patch_event: {err}")),
        }
        let after = http()
            .get(format!(
                "{GRAPH}/me/events/{id}?$select=isOnlineMeeting,onlineMeeting,body"
            ))
            .bearer_auth(token)
            .header("Prefer", "outlook.body-content-type=\"text\"")
            .send()
            .await
            .map_err(|err| format!("GET the edited event: {err}"))?
            .json()
            .await
            .map_err(|err| format!("event json: {err}"))?;
        return Ok((read, after));
    }
    Err("the notes edit kept conflicting".to_owned())
}

#[tokio::test]
async fn live_each_calendar_says_whether_its_server_makes_teams_meetings() {
    let Some(token) = token() else {
        eprintln!("skipping live_each_calendar_says...: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let listed: serde_json::Value = http()
        .get(format!(
            "{GRAPH}/me/calendars?$top=100&$select=allowedOnlineMeetingProviders"
        ))
        .bearer_auth(&token)
        .header("Prefer", "IdType=\"ImmutableId\"")
        .send()
        .await
        .expect("GET /me/calendars")
        .json()
        .await
        .expect("calendars json");
    let calendars = calendar_provider(&token)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars")
        .sync;
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let mut seen_teams = false;
    for calendar in objects {
        let raw = listed["value"]
            .as_array()
            .expect("a calendar list")
            .iter()
            .find(|c| c["id"] == calendar.id.as_str())
            .expect("the adapter's calendar is in Graph's list");
        let providers = &raw["allowedOnlineMeetingProviders"];
        eprintln!("{}: {providers}", calendar.name);
        let lists_teams = providers
            .as_array()
            .is_some_and(|list| list.iter().any(|p| p == "teamsForBusiness"));
        seen_teams |= lists_teams;
        let expected: &[ConferenceService] = if lists_teams {
            &[ConferenceService::Teams]
        } else {
            &[]
        };
        assert_eq!(calendar.native_conferences, expected, "{}", calendar.name);
    }
    if !seen_teams {
        eprintln!("note: no calendar on this account makes Teams meetings");
    }
}

/// The event titled `title`, once the adapter's sync finds it.
async fn read_back(provider: &provider_graph::GraphCalendarProvider, title: &str) -> Option<Event> {
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
    objects.into_iter().find(|event| event.title == title)
}
