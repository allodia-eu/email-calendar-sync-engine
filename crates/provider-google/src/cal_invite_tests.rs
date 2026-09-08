//! Google Calendar invitation request tests.

use engine_core::{calendar::Event, error::FailureClass};
use engine_provider::{
    CalendarAddress, EventDraft, EventPatch, Invitee, InviteePatch, MeetingDraft,
    SchedulingIdentity,
};
use serde_json::json;

use super::{build_create, build_patch};
use crate::{
    cal_normalize::RAW_EVENT_KEY,
    cal_write::cal_write_tests::{base_event, draft, stamp},
};

fn meeting_draft() -> EventDraft {
    draft().meeting(MeetingDraft::new(
        SchedulingIdentity::named(
            CalendarAddress::parse("owner@example.com").unwrap(),
            "Owner",
        ),
        vec![
            Invitee::required(SchedulingIdentity::named(
                CalendarAddress::parse("required@example.com").unwrap(),
                "Required",
            )),
            Invitee::optional(SchedulingIdentity::new(
                CalendarAddress::parse("optional@example.com").unwrap(),
            )),
        ],
    ))
}

fn invited_event() -> Event {
    let mut event = base_event();
    event.extended.set(
        RAW_EVENT_KEY,
        json!({
            "organizer": { "email": "owner@example.com" },
            "attendees": [
                {
                    "email": "keep@example.com",
                    "responseStatus": "accepted",
                    "additionalGuests": 2
                },
                { "email": "remove@example.com", "responseStatus": "tentative" }
            ]
        }),
    );
    event
}

#[test]
fn create_maps_invitees_and_initial_response_state() {
    let body = build_create(&meeting_draft()).unwrap();
    assert_eq!(body["attendees"][0]["email"], "required@example.com");
    assert_eq!(body["attendees"][0]["displayName"], "Required");
    assert_eq!(body["attendees"][0]["responseStatus"], "needsAction");
    assert!(body["attendees"][0].get("optional").is_none());
    assert_eq!(body["attendees"][1]["email"], "optional@example.com");
    assert_eq!(body["attendees"][1]["optional"], true);
}

#[test]
fn patch_preserves_provider_fields_and_existing_responses() {
    let patch = EventPatch::new(stamp()).invitees(
        InviteePatch::new()
            .upsert(Invitee::optional(SchedulingIdentity::new(
                CalendarAddress::parse("keep@example.com").unwrap(),
            )))
            .upsert(Invitee::required(SchedulingIdentity::new(
                CalendarAddress::parse("new@example.com").unwrap(),
            )))
            .remove(CalendarAddress::parse("remove@example.com").unwrap()),
    );
    let body = build_patch(&invited_event(), &patch).unwrap();
    assert_eq!(body["attendees"].as_array().unwrap().len(), 2);
    assert_eq!(body["attendees"][0]["responseStatus"], "accepted");
    assert_eq!(body["attendees"][0]["additionalGuests"], 2);
    assert_eq!(body["attendees"][0]["optional"], true);
    assert_eq!(body["attendees"][1]["responseStatus"], "needsAction");
}

#[test]
fn an_omitted_attendee_list_must_be_fetched_before_editing() {
    let mut event = invited_event();
    event
        .extended
        .set(RAW_EVENT_KEY, json!({ "attendeesOmitted": true }));
    let patch = EventPatch::new(stamp()).invitees(InviteePatch::new().upsert(Invitee::required(
        SchedulingIdentity::new(CalendarAddress::parse("new@example.com").unwrap()),
    )));
    assert_eq!(
        build_patch(&event, &patch).unwrap_err().class(),
        FailureClass::InvalidState
    );
}

#[test]
fn more_than_two_hundred_guests_remains_a_valid_google_request() {
    let invitees = (0..201)
        .map(|index| {
            Invitee::required(SchedulingIdentity::new(
                CalendarAddress::parse(&format!("guest{index}@example.com")).unwrap(),
            ))
        })
        .collect();
    let draft = draft().meeting(MeetingDraft::new(
        SchedulingIdentity::new(CalendarAddress::parse("owner@example.com").unwrap()),
        invitees,
    ));

    let body = build_create(&draft).unwrap();
    assert_eq!(body["attendees"].as_array().unwrap().len(), 201);
}
