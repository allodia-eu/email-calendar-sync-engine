//! Microsoft Graph calendar invitation request tests.

use engine_core::{calendar::Event, error::FailureClass};
use engine_provider::{
    CalendarAddress, EventDraft, EventPatch, Invitee, InviteePatch, MeetingDraft,
    SchedulingIdentity,
};
use serde_json::json;

use super::{build_create, build_patch};
use crate::{
    cal_normalize::RAW_EVENT_KEY,
    cal_write::tests::{base_event, draft, stamp},
};

fn meeting_draft() -> EventDraft {
    draft().meeting(MeetingDraft::new(
        SchedulingIdentity::new(CalendarAddress::parse("owner@example.com").unwrap()),
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
            "organizer": { "emailAddress": { "address": "owner@example.com" } },
            "attendees": [
                {
                    "emailAddress": { "address": "keep@example.com", "name": "Keep" },
                    "type": "required",
                    "status": { "response": "accepted" }
                },
                {
                    "emailAddress": { "address": "remove@example.com" },
                    "type": "required"
                }
            ]
        }),
    );
    event
}

#[test]
fn create_maps_invitees_and_a_stable_transaction_id() {
    let body = build_create(&meeting_draft()).unwrap();
    assert_eq!(
        body["attendees"][0]["emailAddress"]["address"],
        "required@example.com"
    );
    assert_eq!(body["attendees"][0]["emailAddress"]["name"], "Required");
    assert_eq!(body["attendees"][0]["type"], "required");
    assert_eq!(body["attendees"][1]["type"], "optional");
    assert_eq!(body["responseRequested"], true);
    assert_eq!(body["transactionId"], "draft-uid@test.local");
}

#[test]
fn patch_replaces_the_complete_array_without_losing_responses() {
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
    assert_eq!(body["attendees"][0]["status"]["response"], "accepted");
    assert_eq!(body["attendees"][0]["type"], "optional");
    assert_eq!(
        body["attendees"][1]["emailAddress"]["address"],
        "new@example.com"
    );
}

#[test]
fn graph_refuses_more_than_five_hundred_attendees() {
    let invitees = (0..501)
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

    assert_eq!(
        build_create(&draft).unwrap_err().class(),
        FailureClass::InvalidState
    );
}

#[test]
fn roster_patch_cannot_cross_graphs_attendee_limit() {
    let mut event = base_event();
    let attendees = (0..500)
        .map(|index| {
            json!({
                "emailAddress": { "address": format!("guest{index}@example.com") },
                "type": "required"
            })
        })
        .collect::<Vec<_>>();
    event.extended.set(
        RAW_EVENT_KEY,
        json!({
            "organizer": { "emailAddress": { "address": "owner@example.com" } },
            "attendees": attendees
        }),
    );
    let patch = EventPatch::new(stamp()).invitees(InviteePatch::new().upsert(Invitee::required(
        SchedulingIdentity::new(CalendarAddress::parse("overflow@example.com").unwrap()),
    )));

    assert_eq!(
        build_patch(&event, &patch).unwrap_err().class(),
        FailureClass::InvalidState
    );
}
