use engine_core::{calendar::EntryKind, error::FailureClass};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use serde_json::{Value, json};

use super::*;
use crate::test_support::recording_client;

/// `spaces.create`'s answer for a personal account, captured and scrubbed.
const SPACE: &str = include_str!("../tests/fixtures/calendar/meet_space.json");
/// Its answer to a token that does not hold `meetings.space.created`.
const WITHOUT_SCOPE: &str = include_str!("../tests/fixtures/error/meet_space_without_scope.json");
const JOIN: &str = "https://meet.google.com/aaa-bbbb-ccc";

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new("k5x2q7m4n9b3v8c1z6l0p2w4r7").unwrap(),
        title: "Review".into(),
        start: "2026-09-14T09:00:00Z".parse().unwrap(),
        end: "2026-09-14T09:30:00Z".parse().unwrap(),
    }
}

/// The captured answer: a personal account's space has no dial-in numbers.
fn space() -> Value {
    serde_json::from_str(SPACE).unwrap()
}

#[tokio::test]
async fn a_meeting_is_an_empty_space_and_tells_meet_nothing_of_the_event() {
    let (client, requests) = recording_client(vec![("/v2/spaces", Ok(space()))]);
    let meet = MeetMeetings::new(client);
    assert_eq!(meet.service(), ConferenceService::GoogleMeet);

    let made = meet.create_meeting(&request()).await.unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].url, "https://google.test/v2/spaces");
    assert_eq!(requests[0].body, Some(json!({})));

    assert_eq!(made.key, ConferenceKey::new(JOIN));
    assert_eq!(made.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(made.origin, ConferenceOrigin::Attached);
    assert_eq!(made.label.as_deref(), Some("Google Meet"));
    assert_eq!(made.meeting_id.as_deref(), Some("aaa-bbbb-ccc"));
    assert_eq!(made.join().map(ConferenceUri::as_str), Some(JOIN));
    assert_eq!(made.entry_points.len(), 1);
}

#[tokio::test]
async fn a_dial_in_number_carries_its_pin() {
    // The captured answer with the `phoneAccess` the reference documents for a space that
    // has dial-in.
    let mut answer = space();
    answer["phoneAccess"] = json!([
        { "phoneNumber": "+31201234567", "pin": "123456789", "regionCode": "NL" },
        { "phoneNumber": "+31201234567", "pin": "123456789", "regionCode": "NL" },
        { "pin": "1" }
    ]);
    let (client, _) = recording_client(vec![("/v2/spaces", Ok(answer))]);
    let made = MeetMeetings::new(client)
        .create_meeting(&request())
        .await
        .unwrap();

    assert_eq!(made.entry_points.len(), 2);
    let phone = &made.entry_points[1];
    assert_eq!(phone.kind, EntryKind::Phone);
    assert_eq!(phone.uri.as_str(), "tel:+31201234567");
    assert_eq!(phone.label.as_deref(), Some("+31201234567"));
    assert_eq!(phone.access.pin.as_deref(), Some("123456789"));
}

#[tokio::test]
async fn a_space_without_a_link_is_no_meeting() {
    let (client, _) = recording_client(vec![("/v2/spaces", Ok(json!({ "name": "spaces/x" })))]);
    let refused = MeetMeetings::new(client).create_meeting(&request()).await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn a_grant_without_the_scope_is_refused_for_good() {
    let denied = serde_json::from_str(WITHOUT_SCOPE).unwrap();
    let (client, _) = recording_client(vec![("/v2/spaces", Err((403, denied)))]);
    let refused = MeetMeetings::new(client)
        .create_meeting(&request())
        .await
        .unwrap_err();
    assert_eq!(refused.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_delete_sends_nothing() {
    let (client, requests) = recording_client(vec![("/v2/spaces", Ok(space()))]);
    let meet = MeetMeetings::new(client);
    let made = meet.create_meeting(&request()).await.unwrap();
    meet.delete_meeting(&made).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
}
