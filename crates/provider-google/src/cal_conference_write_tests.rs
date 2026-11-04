//! Offline tests for the video-meeting half of a Google calendar write: the `conferenceData` a
//! create and a patch carry, and the `conferenceDataVersion=1` without which Google ignores it.
//!
//! Split from `cal_write_tests` to keep both files under the 500-line cap.

use engine_core::{
    calendar::{
        AccessCodes, Conference, ConferenceKey, ConferenceOrigin, ConferenceService,
        ConferenceText, ConferenceUri, DetectedIn, EntryPoint, Location, NativeState,
    },
    error::FailureClass,
};
use engine_provider::{ConferenceEdit, NewConference, Occurrence};
use serde_json::{Value, json as sjson};

use super::{cal_write_tests::*, *};
use crate::{
    GoogleClient,
    test_support::{capturing_server, fake_client_fallible, retry, tls, user_agent},
};

fn uri(text: &str) -> ConferenceUri {
    ConferenceUri::parse(text).unwrap()
}

pub(super) fn words() -> ConferenceText {
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
        entry_points: vec![join, EntryPoint::at(uri("tel:+31-20-000-0000"))],
        label: Some("Jitsi Meet".into()),
        meeting_id: None,
        notes: None,
    }
}

fn attach() -> NewConference {
    NewConference::attach(jitsi(), words())
}

fn meet_on(mut event: Event) -> Event {
    event.conferences = vec![Conference {
        key: ConferenceKey::new("conferenceData"),
        service: Some(ConferenceService::GoogleMeet),
        origin: ConferenceOrigin::Native {
            removable: true,
            state: NativeState::Ready,
        },
        entry_points: vec![EntryPoint::at(uri("https://meet.google.com/abc-defg-hij"))],
        label: Some("Google Meet".into()),
        meeting_id: None,
        notes: None,
    }];
    event
}

fn patch_body(base: &Event, edit: ConferenceEdit) -> Value {
    build_patch(base, &EventPatch::new(stamp()).conference(edit)).unwrap()
}

#[test]
fn a_create_asks_for_meet_with_a_request_id_a_retry_repeats() {
    let body =
        build_create(&draft().conference(NewConference::Native(ConferenceService::GoogleMeet)))
            .unwrap();
    let request = &body["conferenceData"]["createRequest"];
    assert_eq!(request["conferenceSolutionKey"]["type"], "hangoutsMeet");
    let id = request["requestId"].as_str().unwrap();
    assert!(
        !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()),
        "{id}"
    );
    let again =
        build_create(&draft().conference(NewConference::Native(ConferenceService::GoogleMeet)))
            .unwrap();
    assert_eq!(again["conferenceData"]["createRequest"]["requestId"], id);
}

#[test]
fn a_google_calendar_makes_no_teams_meeting() {
    let err = build_create(&draft().conference(NewConference::Native(ConferenceService::Teams)))
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[test]
fn a_meeting_made_elsewhere_is_an_add_on_with_its_ways_in() {
    let body = build_create(&draft().conference(attach())).unwrap();
    assert_eq!(
        body["conferenceData"],
        sjson!({
            "conferenceSolution": { "key": { "type": "addOn" }, "name": "Jitsi Meet" },
            "entryPoints": [
                { "entryPointType": "video", "uri": "https://meet.example.org/abc123", "passcode": "4321" },
                { "entryPointType": "phone", "uri": "tel:+31-20-000-0000", "label": "+31-20-000-0000" }
            ]
        })
    );
    // Google writes the link into its own invitation, so the notes and location stay ours.
    assert_eq!(body["description"], "agenda");
    assert_eq!(body["location"], "Room A");
}

#[test]
fn an_add_on_without_a_name_takes_the_callers_words() {
    let mut conference = jitsi();
    conference.label = None;
    let new = NewConference::attach(conference, words());
    let body = build_create(&draft().conference(new)).unwrap();
    assert_eq!(
        body["conferenceData"]["conferenceSolution"]["name"],
        "Video meeting"
    );
}

#[test]
fn removing_meet_clears_conference_data_and_leaves_the_notes() {
    let base = meet_on(base_event());
    let body = patch_body(
        &base,
        ConferenceEdit::Remove(ConferenceKey::new("conferenceData")),
    );
    assert_eq!(body, sjson!({ "conferenceData": null }));
}

#[test]
fn replacing_meet_writes_the_new_meeting_in_its_place() {
    let base = meet_on(base_event());
    let body = patch_body(
        &base,
        ConferenceEdit::Set {
            replacing: Some(ConferenceKey::new("conferenceData")),
            new: attach(),
        },
    );
    assert_eq!(
        body["conferenceData"]["conferenceSolution"]["key"]["type"],
        "addOn"
    );
}

#[test]
fn removing_a_meeting_found_in_the_text_takes_it_out_of_the_notes_and_location() {
    let mut base = base_event();
    let block = jitsi().text_block(&words()).unwrap();
    base.description = Some(format!("Bring the figures.\n\n{block}"));
    base.locations = vec![Location::named("https://meet.example.org/abc123")];
    let mut found = jitsi();
    found.origin = ConferenceOrigin::Detected {
        place: DetectedIn::Description,
    };
    base.conferences = vec![found.clone()];
    let body = patch_body(&base, ConferenceEdit::Remove(found.key));
    assert_eq!(
        body,
        sjson!({ "description": "Bring the figures.", "location": "" })
    );
}

#[test]
fn a_meeting_the_event_does_not_hold_cannot_be_removed() {
    let patch =
        EventPatch::new(stamp()).conference(ConferenceEdit::Remove(ConferenceKey::new("nope")));
    assert_eq!(
        build_patch(&base_event(), &patch).unwrap_err().class(),
        FailureClass::InvalidState
    );
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

#[tokio::test]
async fn a_write_with_a_meeting_asks_for_conference_data_version_1() {
    let (base, rx) = capturing_server("200 OK", &stored("x", "u@google.com", "\"v1\"").to_string());
    let client = GoogleClient::with_base("tok", base, tls(), &retry(), &user_agent()).unwrap();
    create_event(&client, "cal-1", &draft().conference(attach()))
        .await
        .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with(
            "POST /calendar/v3/calendars/cal-1/events?sendUpdates=all&conferenceDataVersion=1 "
        ),
        "{request}"
    );
}

#[test]
fn a_new_meeting_on_an_event_with_one_must_name_the_one_it_replaces() {
    // `conferenceData` holds one meeting; writing another without naming it would drop it
    // unannounced.
    let patch = EventPatch::new(stamp()).conference(ConferenceEdit::Set {
        replacing: None,
        new: attach(),
    });
    let err = build_patch(&meet_on(base_event()), &patch).unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[test]
fn a_location_the_same_patch_sets_survives_the_removal() {
    let mut base = base_event();
    base.locations = vec![Location::named("https://meet.example.org/abc123")];
    let mut found = jitsi();
    found.origin = ConferenceOrigin::Detected {
        place: DetectedIn::Location,
    };
    base.conferences = vec![found.clone()];
    let patch = EventPatch::new(stamp())
        .location("https://meet.example.org/abc123")
        .conference(ConferenceEdit::Remove(found.key));
    let body = build_patch(&base, &patch).unwrap();
    assert_eq!(
        body,
        sjson!({ "location": "https://meet.example.org/abc123" })
    );
}
