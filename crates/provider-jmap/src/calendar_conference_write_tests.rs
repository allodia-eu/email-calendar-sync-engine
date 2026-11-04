//! The JSON a meeting write sends, asserted literally: the fake executor answers whatever it
//! is sent, so only the request shows what the server would be asked. Whether a server keeps
//! it is `tests/live_calendar_conference_write.rs`.

use engine_core::{
    calendar::{ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceText, EntryPoint},
    error::FailureClass,
};
use engine_provider::{
    CalendarWrites, ConferenceEdit, EventDraft, EventEdit, EventPatch, Occurrence, PatchTarget,
};
use serde_json::{Value, json};

use super::*;
use crate::{calendar::event_from_json, provider::provider_test_support::*};

const JITSI: &str = "https://meet.example.org/planning-4711";
const TALK: &str = "https://cloud.example.com/call/abcd1234";
const BLOCK: &str = "-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=";

fn words() -> ConferenceText {
    ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
        dial_in_code: "Conference ID".into(),
    }
}

fn jitsi() -> NewConference {
    NewConference::attach(
        Conference {
            key: ConferenceKey::new(JITSI),
            service: Some(ConferenceService::Jitsi),
            origin: ConferenceOrigin::Attached,
            entry_points: vec![EntryPoint::at(ConferenceUri::parse(JITSI).unwrap())],
            label: Some("Jitsi Meet".into()),
            meeting_id: None,
            notes: None,
        },
        words(),
    )
}

fn jitsi_block() -> String {
    format!("{BLOCK}\nJitsi Meet\nJoin: {JITSI}\n{BLOCK}")
}

/// An event as the server returns it, read through the adapter's own parser.
fn event(extra: &Value) -> Event {
    let mut raw = json!({
        "@type": "Event", "id": "l", "uid": "evt-1@test.local", "calendarIds": { "b": true },
        "title": "Review", "start": "2026-08-01T09:00:00", "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
    });
    raw.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    event_from_json(&raw).unwrap()
}

/// An event carrying a Talk meeting as this adapter writes one.
fn with_talk() -> Event {
    event(&json!({
        "description": format!(
            "Bring the figures.\n\n{BLOCK}\nNextcloud Talk\nJoin: {TALK}\nDial in: +31 20 1234567\n{BLOCK}"
        ),
        "locations": { "1": { "@type": "Location", "name": TALK } },
        "virtualLocations": {
            "meeting-1": { "@type": "VirtualLocation", "uri": TALK, "name": "Nextcloud Talk" },
            "meeting-2": { "@type": "VirtualLocation", "uri": "tel:+31201234567" },
        },
    }))
}

fn update(base: &Event, patch: EventPatch) -> Map<String, Value> {
    crate::calendar_patch::patch_to_json(
        base,
        &EventEdit::new(base, PatchTarget::Series, patch),
        None,
    )
    .unwrap()
}

fn patch() -> EventPatch {
    EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap())
}

fn attach(new: NewConference) -> ConferenceEdit {
    ConferenceEdit::Set {
        replacing: None,
        new,
    }
}

#[tokio::test]
async fn a_create_sends_the_meeting_its_block_and_its_link_as_the_location() {
    let (p, exec) = recording(vec![json!({ "methodResponses": [[
        "CalendarEvent/set", { "created": { "new": { "id": "l" } } }, "0"
    ]] })]);
    let draft = EventDraft::new(
        engine_core::ids::CalendarId::try_from("b").unwrap(),
        engine_core::ids::Uid::new("evt-1@test.local").unwrap(),
        "Review",
        event(&json!({})).start,
        event(&json!({})).start,
        "2026-07-14T10:00:00Z".parse().unwrap(),
    )
    .description("Bring the figures.")
    .conference(jitsi());
    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    let object = &args["create"]["new"];
    assert_eq!(
        object["description"],
        json!(format!("Bring the figures.\n\n{}", jitsi_block()))
    );
    assert_eq!(
        object["locations"],
        json!({ "1": { "@type": "Location", "name": JITSI } })
    );
    assert_eq!(
        object["virtualLocations"],
        json!({ "meeting-1": {
            "@type": "VirtualLocation", "uri": JITSI, "name": "Jitsi Meet",
            "features": { "audio": true, "video": true },
        } })
    );
}

#[test]
fn attaching_to_an_event_without_meetings_assigns_the_whole_map() {
    let base = event(&json!({ "locations": { "x": { "@type": "Location", "name": "Room 4" } } }));
    let update = update(&base, patch().conference(attach(jitsi())));
    assert_eq!(update["description"], json!(jitsi_block()));
    assert!(update.get("locations/x/name").is_none(), "{update:?}");
    assert_eq!(
        update["virtualLocations"]["meeting-1"]["uri"],
        json!(JITSI),
        "{update:?}"
    );
}

#[test]
fn removing_a_meeting_takes_its_entries_block_and_location() {
    let base = with_talk();
    assert_eq!(base.conferences.len(), 1, "{:?}", base.conferences);
    let key = base.conferences[0].key.clone();
    assert_eq!(key.as_str(), "meeting-1");
    let update = update(&base, patch().conference(ConferenceEdit::Remove(key)));
    assert_eq!(
        Value::Object(update),
        json!({
            "description": "Bring the figures.",
            "locations/1": null,
            "virtualLocations/meeting-1": null,
            "virtualLocations/meeting-2": null,
        }),
    );
}

#[test]
fn replacing_a_meeting_writes_the_new_one_at_a_fresh_id() {
    let base = with_talk();
    let key = base.conferences[0].key.clone();
    let update = update(
        &base,
        patch().conference(ConferenceEdit::Set {
            replacing: Some(key),
            new: jitsi(),
        }),
    );
    assert_eq!(update["virtualLocations/meeting-1"], Value::Null);
    assert_eq!(update["virtualLocations/meeting-3"]["uri"], json!(JITSI));
    // The dial-in was the old meeting's, so it goes with it.
    assert_eq!(update["virtualLocations/meeting-2"], Value::Null);
    // The old link was the location, so the new one takes its place.
    assert_eq!(update["locations/1/name"], json!(JITSI));
    assert_eq!(
        update["description"],
        json!(format!("Bring the figures.\n\n{}", jitsi_block()))
    );
}

#[test]
fn a_location_the_edit_sets_survives_the_meetings_removal() {
    let base = with_talk();
    let key = base.conferences[0].key.clone();
    let update = update(
        &base,
        patch()
            .location("Room 4")
            .conference(ConferenceEdit::Remove(key)),
    );
    assert_eq!(update["locations/1/name"], json!("Room 4"));
    assert!(update.get("locations/1").is_none());
}

#[test]
fn attaching_a_meeting_again_does_not_repeat_it() {
    let base = event(&json!({
        "description": jitsi_block(),
        "locations": { "1": { "@type": "Location", "name": JITSI } },
        "virtualLocations": { "meeting-1": { "@type": "VirtualLocation", "uri": JITSI } },
    }));
    let update = update(&base, patch().conference(attach(jitsi())));
    assert_eq!(update["virtualLocations/meeting-1"], Value::Null);
    assert_eq!(update["virtualLocations/meeting-2"]["uri"], json!(JITSI));
    // The notes already read this way, so they are not written.
    assert!(update.get("description").is_none(), "{update:?}");
}

#[tokio::test]
async fn a_meeting_on_one_occurrence_is_refused_before_anything_is_sent() {
    let (p, exec) = recording(Vec::new());
    let base = with_talk();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Instance(Occurrence::starting(base.start.clone())),
        patch().conference(attach(jitsi())),
    );
    let error = p.patch_event(&account(), &base, &edit).await.unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
    assert_eq!(exec.request_count(), 0);
}

#[tokio::test]
async fn a_meeting_the_server_would_make_is_refused_on_a_patch() {
    let (p, exec) = recording(Vec::new());
    let base = with_talk();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        patch().conference(attach(NewConference::Native(ConferenceService::Teams))),
    );
    let error = p.patch_event(&account(), &base, &edit).await.unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
    assert_eq!(exec.request_count(), 0);
}

#[test]
fn a_meetings_notes_ride_on_its_join_link() {
    let NewConference::Attach(mut attached) = jitsi() else {
        unreachable!()
    };
    attached.conference.notes = Some("Open in a browser".into());
    let update = update(
        &event(&json!({})),
        patch().conference(attach(NewConference::Attach(attached))),
    );
    let entry = &update["virtualLocations"]["meeting-1"];
    assert_eq!(entry["description"], json!("Open in a browser"));
    // The reader takes the meeting's notes from there.
    let mut raw: Value = json!({
        "@type": "Event", "id": "l", "uid": "evt-1@test.local", "calendarIds": { "b": true },
        "title": "Review", "start": "2026-08-01T09:00:00", "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
    });
    raw["virtualLocations"] = update["virtualLocations"].clone();
    let read = event_from_json(&raw).unwrap();
    assert_eq!(
        read.conferences[0].notes.as_deref(),
        Some("Open in a browser")
    );
}

#[test]
fn removing_a_meeting_written_only_in_the_text_writes_no_entries() {
    // Graph and pasted invitations leave a meeting only in the notes and location; the
    // event then has no `virtualLocations` map to point into.
    let base = event(&json!({
        "description": format!("Agenda\n\n{}", jitsi_block()),
        "locations": { "1": { "@type": "Location", "name": JITSI } },
    }));
    assert_eq!(base.conferences.len(), 1, "{:?}", base.conferences);
    let key = base.conferences[0].key.clone();
    let update = update(&base, patch().conference(ConferenceEdit::Remove(key)));
    assert_eq!(
        Value::Object(update),
        json!({ "description": "Agenda", "locations/1": null })
    );
}

#[test]
fn a_dial_in_keeps_its_own_description() {
    let NewConference::Attach(mut attached) = jitsi() else {
        unreachable!()
    };
    let mut dial = EntryPoint::at(ConferenceUri::parse("tel:+31201234567").unwrap());
    dial.description = Some("Then enter 4711#".into());
    attached.conference.entry_points.push(dial);
    let update = update(
        &event(&json!({})),
        patch().conference(attach(NewConference::Attach(attached))),
    );
    assert_eq!(
        update["virtualLocations"]["meeting-2"]["description"],
        json!("Then enter 4711#")
    );
    assert!(
        update["virtualLocations"]["meeting-1"]
            .get("description")
            .is_none()
    );
}

#[test]
fn the_patch_object_refuses_a_meeting_on_one_occurrence() {
    let base = with_talk();
    let edit = EventEdit::new(
        &base,
        PatchTarget::Instance(Occurrence::starting(base.start.clone())),
        patch().conference(ConferenceEdit::Remove(ConferenceKey::new("unknown"))),
    );
    let error = crate::calendar_patch::patch_to_json(&base, &edit, None).unwrap_err();
    assert!(error.to_string().contains("whole series"), "{error}");
}

#[test]
fn a_create_asking_the_server_for_a_meeting_is_refused() {
    let draft = EventDraft::new(
        engine_core::ids::CalendarId::try_from("b").unwrap(),
        engine_core::ids::Uid::new("evt-1@test.local").unwrap(),
        "Review",
        event(&json!({})).start,
        event(&json!({})).start,
        "2026-07-14T10:00:00Z".parse().unwrap(),
    )
    .conference(NewConference::Native(ConferenceService::Teams));
    let error = apply_to_create(&draft, &mut Map::new()).unwrap_err();
    assert!(
        error.to_string().contains("no meetings of its own"),
        "{error}"
    );
}
