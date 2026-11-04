//! Offline tests for the video-meeting half of a Graph calendar write: asking Exchange for
//! Teams, taking a Teams meeting off by its block, and the block of ours a meeting made
//! elsewhere is written as.
//!
//! Split from `cal_write_tests` to keep both files under the 500-line cap.

use engine_core::{
    calendar::{
        AccessCodes, Conference, ConferenceKey, ConferenceOrigin, ConferenceService,
        ConferenceText, ConferenceUri, DetectedIn, EntryPoint, Location,
    },
    error::FailureClass,
};
use engine_provider::{ConferenceEdit, NewConference, Occurrence};
use serde_json::{Value, json as sjson};

use super::{tests::*, *};
use crate::{cal_normalize::event_from_json, test_support::fake_client_fallible};

const TEAMS: &str = include_str!("../tests/fixtures/calendar/event_online_meeting_text_body.json");

fn uri(text: &str) -> ConferenceUri {
    ConferenceUri::parse(text).unwrap()
}

fn words() -> ConferenceText {
    ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
    }
}

fn jitsi() -> Conference {
    let mut join = EntryPoint::at(uri("https://meet.example.org/abc123"));
    join.access = AccessCodes {
        passcode: Some("4321".into()),
        ..AccessCodes::default()
    };
    Conference {
        key: ConferenceKey::new("https://meet.example.org/abc123"),
        service: Some(ConferenceService::Jitsi),
        origin: ConferenceOrigin::Attached,
        entry_points: vec![join],
        label: Some("Jitsi Meet".into()),
        meeting_id: None,
        notes: None,
    }
}

fn attach() -> NewConference {
    NewConference::attach(jitsi(), words())
}

/// A captured Teams meeting on a work tenant: Teams' block in the body, Exchange's stand-in
/// location, the meeting itself in `onlineMeeting`.
fn teams_meeting() -> Event {
    let value: Value = serde_json::from_str(TEAMS).unwrap();
    event_from_json(&value, &calendar()).unwrap()
}

fn patch_body(base: &Event, patch: &EventPatch) -> Value {
    build_patch(base, patch).unwrap()
}

#[test]
fn a_create_asks_exchange_for_a_teams_meeting() {
    let body =
        build_create(&draft().conference(NewConference::Native(ConferenceService::Teams))).unwrap();
    assert_eq!(body["isOnlineMeeting"], true);
    assert_eq!(body["onlineMeetingProvider"], "teamsForBusiness");
    // Exchange appends its own block, so the notes go as they are.
    assert_eq!(
        body["body"],
        sjson!({ "contentType": "text", "content": "agenda" })
    );
}

#[test]
fn an_exchange_calendar_makes_no_meet_meeting() {
    let err =
        build_create(&draft().conference(NewConference::Native(ConferenceService::GoogleMeet)))
            .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[test]
fn a_meeting_made_elsewhere_goes_into_the_notes_and_an_empty_location() {
    let block = jitsi().text_block(&words()).unwrap();
    let mut plain = draft().conference(attach());
    plain.location = None;
    let body = build_create(&plain).unwrap();
    assert_eq!(body["body"]["content"], format!("agenda\n\n{block}"));
    assert_eq!(
        body["location"],
        sjson!({ "displayName": "https://meet.example.org/abc123" })
    );
    assert!(body.get("isOnlineMeeting").is_none());
    // A location the person typed stays.
    let body = build_create(&draft().conference(attach())).unwrap();
    assert_eq!(body["location"], sjson!({ "displayName": "Room A" }));
}

#[test]
fn removing_teams_writes_the_notes_without_its_block_and_clears_the_stand_in() {
    let base = teams_meeting();
    let body = patch_body(
        &base,
        &EventPatch::new(stamp())
            .conference(ConferenceEdit::Remove(ConferenceKey::new("onlineMeeting"))),
    );
    let notes = body["body"]["content"].as_str().unwrap();
    assert!(
        notes.starts_with("Agenda for the quarterly review"),
        "{notes}"
    );
    assert!(notes.ends_with("decisions and owners."), "{notes}");
    assert!(!notes.contains("teams.microsoft.com"), "{notes}");
    assert_eq!(body["location"], sjson!({ "displayName": "" }));
    assert!(
        body.get("isOnlineMeeting").is_none(),
        "Graph ignores it; the block is the switch"
    );
}

#[test]
fn a_notes_edit_beside_the_removal_is_what_loses_the_block() {
    let base = teams_meeting();
    let edited = base
        .description
        .clone()
        .unwrap()
        .replacen("Agenda for", "Revised agenda for", 1);
    let body = patch_body(
        &base,
        &EventPatch::new(stamp())
            .description(edited)
            .conference(ConferenceEdit::Remove(ConferenceKey::new("onlineMeeting"))),
    );
    let notes = body["body"]["content"].as_str().unwrap();
    assert!(notes.starts_with("Revised agenda for"), "{notes}");
    assert!(!notes.contains("Microsoft Teams"), "{notes}");
}

#[test]
fn replacing_teams_with_a_meeting_made_elsewhere_swaps_the_blocks_and_the_location() {
    let base = teams_meeting();
    let block = jitsi().text_block(&words()).unwrap();
    let body = patch_body(
        &base,
        &EventPatch::new(stamp()).conference(ConferenceEdit::Set {
            replacing: Some(ConferenceKey::new("onlineMeeting")),
            new: attach(),
        }),
    );
    let notes = body["body"]["content"].as_str().unwrap();
    assert!(
        notes.ends_with(&format!("decisions and owners.\n\n{block}")),
        "{notes}"
    );
    assert_eq!(
        body["location"],
        sjson!({ "displayName": "https://meet.example.org/abc123" })
    );
}

#[test]
fn adding_teams_to_an_event_asks_for_it_and_leaves_the_notes_alone() {
    let body = patch_body(
        &base_event(),
        &EventPatch::new(stamp()).conference(ConferenceEdit::Set {
            replacing: None,
            new: NewConference::Native(ConferenceService::Teams),
        }),
    );
    assert_eq!(
        body,
        sjson!({ "isOnlineMeeting": true, "onlineMeetingProvider": "teamsForBusiness" })
    );
}

#[test]
fn removing_a_meeting_of_ours_takes_its_block_and_its_link_as_location() {
    let mut base = base_event();
    let block = jitsi().text_block(&words()).unwrap();
    base.description = Some(format!("Bring the figures.\n\n{block}"));
    base.locations = vec![Location::named("https://meet.example.org/abc123")];
    base.extended.set(
        crate::cal_normalize::RAW_EVENT_KEY,
        sjson!({ "location": { "displayName": "https://meet.example.org/abc123" } }),
    );
    let mut found = jitsi();
    found.origin = ConferenceOrigin::Detected {
        place: DetectedIn::Description,
    };
    base.conferences = vec![found.clone()];
    let body = patch_body(
        &base,
        &EventPatch::new(stamp()).conference(ConferenceEdit::Remove(found.key)),
    );
    assert_eq!(body["body"]["content"], "Bring the figures.");
    assert_eq!(body["location"], sjson!({ "displayName": "" }));
}

#[tokio::test]
async fn an_occurrence_cannot_carry_a_meeting_edit() {
    let client = fake_client_fallible(vec![]);
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Instance(Occurrence::starting(zoned("2026-08-10T09:00:00"))),
        EventPatch::new(stamp()).conference(ConferenceEdit::Set {
            replacing: None,
            new: attach(),
        }),
    );
    let err = patch_event(&client, &base_event(), &edit)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("belongs to the series"), "{err}");
}

#[test]
fn a_location_the_same_patch_sets_survives_the_removal() {
    let base = teams_meeting();
    let body = patch_body(
        &base,
        &EventPatch::new(stamp())
            .location("Room 4")
            .conference(ConferenceEdit::Remove(ConferenceKey::new("onlineMeeting"))),
    );
    assert_eq!(body["location"], sjson!({ "displayName": "Room 4" }));
}

#[test]
fn a_removal_that_takes_nothing_from_the_notes_leaves_the_body_alone() {
    // A meeting found only in the location: the notes stay as Exchange holds them, HTML and
    // all, rather than being rewritten as text.
    let mut base = base_event();
    base.description = Some("Bring the figures.".into());
    base.extended.set(
        crate::cal_normalize::RAW_EVENT_KEY,
        sjson!({ "location": { "displayName": "https://meet.example.org/abc123" } }),
    );
    let mut found = jitsi();
    found.origin = ConferenceOrigin::Detected {
        place: DetectedIn::Location,
    };
    base.conferences = vec![found.clone()];
    let body = patch_body(
        &base,
        &EventPatch::new(stamp()).conference(ConferenceEdit::Remove(found.key)),
    );
    assert_eq!(body, sjson!({ "location": { "displayName": "" } }));
}

#[test]
fn attaching_a_meeting_the_notes_already_carry_writes_its_block_once() {
    let mut base = base_event();
    let block = jitsi().text_block(&words()).unwrap();
    base.description = Some(format!("Bring the figures.\n\n{block}"));
    let body = patch_body(
        &base,
        &EventPatch::new(stamp()).conference(ConferenceEdit::Set {
            replacing: None,
            new: attach(),
        }),
    );
    // Replacing the block with itself leaves the notes as they are, so they are not written.
    assert!(body.get("body").is_none(), "{body}");
}
