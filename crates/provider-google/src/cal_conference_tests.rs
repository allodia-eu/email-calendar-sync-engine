use serde_json::json;

use super::*;

const MEET: &str = include_str!("../tests/fixtures/calendar/event_meet.json");

fn only(value: &Value) -> Conference {
    let mut found = conferences(value);
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

#[test]
fn a_meet_conference_is_the_calendars_own_and_can_be_taken_off() {
    let meet = only(&serde_json::from_str(MEET).unwrap());
    assert_eq!(meet.key.as_str(), "conferenceData");
    assert_eq!(meet.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(meet.origin, native(NativeState::Ready));
    assert_eq!(meet.label.as_deref(), Some("Google Meet"));
    assert_eq!(meet.meeting_id.as_deref(), Some("aaa-bbbb-ccc"));
    assert_eq!(
        meet.join().map(ConferenceUri::as_str),
        Some("https://meet.google.com/aaa-bbbb-ccc")
    );
    assert_eq!(
        meet.entry_points[0].label.as_deref(),
        Some("meet.google.com/aaa-bbbb-ccc")
    );
}

#[test]
fn every_way_in_keeps_its_codes_and_the_video_one_leads() {
    // The shape of Google's reference example, with the phone entry listed first.
    let event = json!({ "conferenceData": {
        "conferenceSolution": { "key": { "type": "hangoutsMeet" }, "name": "Google Meet" },
        "conferenceId": "aaa-bbbb-ccc",
        "notes": "<b>Dial</b> in<br>from a phone",
        "entryPoints": [
            { "entryPointType": "phone", "uri": "tel:+31-20-000-0000", "label": "+31 20 000 0000", "pin": "123456789" },
            { "entryPointType": "video", "uri": "https://meet.google.com/aaa-bbbb-ccc", "meetingCode": "aaa-bbbb-ccc" },
            { "entryPointType": "sip", "uri": "sip:123456789@meet.example.com", "accessCode": "4321", "passcode": "p", "password": "pw" },
            { "entryPointType": "more", "uri": "https://tel.meet/aaa-bbbb-ccc?pin=1" },
            { "entryPointType": "video", "uri": "javascript:alert(1)" }
        ]
    }});
    let meet = only(&event);
    let kinds: Vec<EntryKind> = meet.entry_points.iter().map(|e| e.kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            EntryKind::Video,
            EntryKind::Phone,
            EntryKind::Sip,
            EntryKind::More
        ]
    );
    assert_eq!(
        meet.entry_points[0].access.meeting_code.as_deref(),
        Some("aaa-bbbb-ccc")
    );
    assert_eq!(
        meet.entry_points[1].access.pin.as_deref(),
        Some("123456789")
    );
    assert_eq!(
        meet.entry_points[1].label.as_deref(),
        Some("+31 20 000 0000")
    );
    let sip = &meet.entry_points[2].access;
    assert_eq!(
        (
            sip.access_code.as_deref(),
            sip.passcode.as_deref(),
            sip.password.as_deref()
        ),
        (Some("4321"), Some("p"), Some("pw"))
    );
    assert_eq!(meet.notes.as_deref(), Some("Dial in\nfrom a phone"));
}

#[test]
fn a_meet_request_google_has_not_finished_is_pending_with_no_way_in() {
    let event = json!({ "conferenceData": {
        "createRequest": {
            "requestId": "r-1",
            "conferenceSolutionKey": { "type": "hangoutsMeet" },
            "status": { "statusCode": "pending" }
        }
    }});
    let meet = only(&event);
    assert_eq!(meet.origin, native(NativeState::Pending));
    assert_eq!(meet.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(meet.entry_points, []);

    let mut failed = event;
    failed["conferenceData"]["createRequest"]["status"]["statusCode"] = json!("failure");
    assert_eq!(only(&failed).origin, native(NativeState::Failed));
}

#[test]
fn an_add_on_conference_is_stored_not_made_and_named_by_its_link() {
    let event = json!({ "conferenceData": {
        "conferenceSolution": { "key": { "type": "addOn" }, "name": "Zoom Meeting" },
        "conferenceId": "81234567890",
        "entryPoints": [
            { "entryPointType": "video", "uri": "https://us02web.zoom.us/j/81234567890?pwd=x", "passcode": "123456" }
        ]
    }});
    let zoom = only(&event);
    assert_eq!(zoom.origin, ConferenceOrigin::Attached);
    assert_eq!(zoom.service, Some(ConferenceService::Zoom));
    assert_eq!(zoom.label.as_deref(), Some("Zoom Meeting"));
    assert_eq!(
        zoom.entry_points[0].access.passcode.as_deref(),
        Some("123456")
    );
}

#[test]
fn a_hangout_link_without_conference_data_is_a_meet_conference() {
    let meet = only(&json!({ "hangoutLink": "https://meet.google.com/abc-defg-hij" }));
    assert_eq!(meet.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(meet.origin, native(NativeState::Ready));
    assert_eq!(meet.entry_points.len(), 1);
}

#[test]
fn conference_data_with_nothing_usable_falls_back_to_the_hangout_link() {
    let link = "https://meet.google.com/abc-defg-hij";
    for data in [
        json!({ "entryPoints": [] }),
        json!({ "entryPoints": [{ "entryPointType": "video", "uri": "not a uri" }] }),
        Value::Null,
    ] {
        let meet = only(&json!({ "hangoutLink": link, "conferenceData": data }));
        assert_eq!(meet.join().map(ConferenceUri::as_str), Some(link), "{data}");
    }
}

#[test]
fn an_add_on_request_still_in_flight_is_no_conference() {
    let event = json!({ "conferenceData": {
        "conferenceSolution": { "key": { "type": "addOn" }, "name": "Zoom Meeting" },
        "createRequest": { "requestId": "r-2", "status": { "statusCode": "pending" } }
    }});
    assert_eq!(conferences(&event), []);
}

#[test]
fn conference_data_with_nothing_to_join_and_nothing_pending_is_no_conference() {
    assert_eq!(
        conferences(&json!({ "conferenceData": { "entryPoints": [] } })),
        []
    );
    assert_eq!(conferences(&json!({ "hangoutLink": "" })), []);
    assert_eq!(conferences(&json!({})), []);
}
