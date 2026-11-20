use engine_core::calendar::{ConferenceUri, EntryKind};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use serde_json::{Value, json};

use super::*;
use crate::{
    principal::MailboxPrincipal,
    test_support::{FakeRoute, fake_client_logged},
};

const KEY: &str = "k5x2q7m4n9b3v8c1z6l0p2w4r7";
/// `createOrGet`'s answer, captured from a work account and scrubbed.
const MEETING: &str = include_str!("../tests/fixtures/calendar/online_meeting.json");
/// The `JoinWebUrl` lookup of that meeting, captured with it.
const LOOKUP: &str = include_str!("../tests/fixtures/calendar/online_meeting_lookup.json");
/// Graph's answer to a lookup or a delete of a meeting that is gone.
const NOT_FOUND: &str = include_str!("../tests/fixtures/error/online_meeting_not_found.json");
const JOIN: &str = "https://teams.microsoft.com/l/meetup-join/19%3Ameeting_Zml4dHVyZS1tZWV0aW5n%40thread.v2/0?context=%7b%22Tid%22%3a%2200000000-0000-0000-0000-00000000cafe%22%2c%22Oid%22%3a%2200000000-0000-0000-0000-00000000feed%22%7d";
const ID: &str = "fixture-online-meeting-1";

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new(KEY).unwrap(),
        title: "Review".into(),
        start: "2026-09-14T09:00:00Z".parse().unwrap(),
        end: "2026-09-14T09:30:00Z".parse().unwrap(),
    }
}

/// The captured answer: an account without Audio Conferencing has no dial-in numbers.
fn answer() -> Value {
    serde_json::from_str(MEETING).unwrap()
}

/// The captured answer with the `audioConferencing` Graph documents for a licensed account.
fn answer_with_numbers() -> Value {
    let mut meeting = answer();
    meeting["audioConferencing"] = json!({
        "conferenceId": "123456789",
        "tollNumber": "+31 20 123 4567",
        "tollFreeNumbers": ["+31 800 000 0000", "+31 20 123 4567"]
    });
    meeting
}

fn lookup() -> Value {
    serde_json::from_str(LOOKUP).unwrap()
}

fn gone() -> FakeRoute {
    Err((404, serde_json::from_str(NOT_FOUND).unwrap()))
}

fn service(routes: Vec<(&str, FakeRoute)>) -> (TeamsMeetings, crate::test_support::RequestLog) {
    let (client, _, requests) = fake_client_logged(routes);
    (TeamsMeetings::new(client), requests)
}

#[tokio::test]
async fn a_meeting_is_made_with_the_key_as_its_external_id() {
    let (teams, requests) = service(vec![("/me/onlineMeetings/createOrGet", Ok(answer()))]);
    assert_eq!(teams.service(), ConferenceService::Teams);
    teams.create_meeting(&request()).await.unwrap();
    let requests = requests.lock().unwrap();
    let (method, url, body) = &requests[0];
    assert_eq!(*method, "POST");
    assert_eq!(url, "https://graph.test/me/onlineMeetings/createOrGet");
    assert_eq!(
        body.as_ref().unwrap(),
        &json!({
            "externalId": KEY,
            "subject": "Review",
            "startDateTime": "2026-09-14T09:00:00Z",
            "endDateTime": "2026-09-14T09:30:00Z",
        })
    );
}

#[tokio::test]
async fn a_client_rooted_at_a_shared_mailbox_makes_the_meeting_as_its_own_user() {
    let (client, _, requests) =
        fake_client_logged(vec![("/me/onlineMeetings/createOrGet", Ok(answer()))]);
    let shared = MailboxPrincipal::user("team@example.org").unwrap();
    let teams = TeamsMeetings::new(client.with_principal(shared));
    teams.create_meeting(&request()).await.unwrap();
    assert_eq!(
        requests.lock().unwrap()[0].1,
        "https://graph.test/me/onlineMeetings/createOrGet"
    );
}

#[tokio::test]
async fn the_answer_is_a_meeting_made_elsewhere_with_its_codes_and_numbers() {
    let (teams, _) = service(vec![("createOrGet", Ok(answer_with_numbers()))]);
    let meeting = teams.create_meeting(&request()).await.unwrap();
    assert_eq!(meeting.key.as_str(), JOIN);
    assert_eq!(meeting.service, Some(ConferenceService::Teams));
    assert_eq!(meeting.origin, ConferenceOrigin::Attached);
    assert_eq!(meeting.label.as_deref(), Some("Microsoft Teams"));
    assert_eq!(meeting.meeting_id.as_deref(), Some("1234567890123456"));
    let join = &meeting.entry_points[0];
    assert_eq!(join.uri.as_str(), JOIN);
    assert_eq!(join.access.passcode.as_deref(), Some("Fx7pQ2zA"));
    let numbers: Vec<_> = meeting.entry_points[1..]
        .iter()
        .map(|entry| (entry.kind.clone(), entry.uri.as_str().to_owned()))
        .collect();
    assert_eq!(
        numbers,
        [
            (EntryKind::Phone, "tel:+31201234567".to_owned()),
            (EntryKind::Phone, "tel:+318000000000".to_owned()),
        ]
    );
    for entry in &meeting.entry_points[1..] {
        assert_eq!(entry.access.access_code.as_deref(), Some("123456789"));
    }
    assert_eq!(join.access.access_code, None, "the phone ID is no web code");
}

#[tokio::test]
async fn an_account_without_audio_conferencing_has_only_the_link() {
    let (teams, _) = service(vec![("createOrGet", Ok(answer()))]);
    let meeting = teams.create_meeting(&request()).await.unwrap();
    assert_eq!(meeting.entry_points.len(), 1);
}

#[tokio::test]
async fn a_meeting_without_a_passcode_has_none() {
    let mut open = answer();
    open["joinMeetingIdSettings"]["passcode"] = Value::Null;
    let (teams, _) = service(vec![("createOrGet", Ok(open))]);
    let meeting = teams.create_meeting(&request()).await.unwrap();
    assert_eq!(meeting.entry_points[0].access.passcode, None);
}

#[tokio::test]
async fn an_answer_without_a_join_link_is_refused() {
    let (teams, _) = service(vec![("createOrGet", Ok(json!({ "id": "x" })))]);
    assert!(teams.create_meeting(&request()).await.is_err());
    let (teams, _) = service(vec![("createOrGet", Ok(Value::Null))]);
    assert!(teams.create_meeting(&request()).await.is_err());
}

#[tokio::test]
async fn a_meeting_is_found_by_its_join_link_and_deleted() {
    let (teams, requests) = service(vec![("$filter=", Ok(lookup())), (ID, Ok(Value::Null))]);
    let meeting = conference(&answer()).unwrap();
    teams.delete_meeting(&meeting).await.unwrap();
    let requests = requests.lock().unwrap();
    let (method, url, _) = &requests[0];
    assert_eq!(*method, "GET");
    let filter = url.split_once("$filter=").unwrap().1;
    assert_eq!(filter, encode(&format!("JoinWebUrl eq '{JOIN}'")));
    assert_eq!(requests[1].0, "DELETE");
    assert_eq!(
        requests[1].1,
        format!("https://graph.test/me/onlineMeetings/{ID}")
    );
}

#[tokio::test]
async fn a_meeting_read_back_from_an_event_is_deleted_by_its_link_alone() {
    let (teams, requests) = service(vec![("$filter=", Ok(lookup())), (ID, Ok(Value::Null))]);
    let read_back = Conference {
        key: ConferenceKey::new("meeting-1"),
        service: None,
        origin: ConferenceOrigin::Attached,
        entry_points: vec![EntryPoint::at(ConferenceUri::parse(JOIN).unwrap())],
        label: None,
        meeting_id: None,
        notes: None,
    };
    teams.delete_meeting(&read_back).await.unwrap();
    assert_eq!(requests.lock().unwrap()[1].0, "DELETE");
}

#[tokio::test]
async fn a_meeting_already_gone_is_deleted() {
    let meeting = conference(&answer()).unwrap();

    let (teams, requests) = service(vec![("$filter=", gone())]);
    teams.delete_meeting(&meeting).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (teams, requests) = service(vec![("$filter=", Ok(json!({ "value": [] })))]);
    teams.delete_meeting(&meeting).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (teams, _) = service(vec![("$filter=", Ok(lookup())), (ID, gone())]);
    teams.delete_meeting(&meeting).await.unwrap();
}

#[tokio::test]
async fn a_refused_delete_is_an_error() {
    let refused = || Err((403, json!({ "error": { "code": "Forbidden" } })));
    let meeting = conference(&answer()).unwrap();
    let (teams, _) = service(vec![("$filter=", refused())]);
    assert!(teams.delete_meeting(&meeting).await.is_err());
    let (teams, _) = service(vec![("$filter=", Ok(lookup())), (ID, refused())]);
    assert!(teams.delete_meeting(&meeting).await.is_err());
}

#[tokio::test]
async fn a_meeting_without_a_join_link_cannot_be_found() {
    let (teams, requests) = service(vec![]);
    let mut meeting = conference(&answer()).unwrap();
    meeting.entry_points.remove(0);
    let err = teams.delete_meeting(&meeting).await.unwrap_err();
    assert!(err.to_string().contains("join link"), "{err}");
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn the_notes_tell_a_caller_the_phone_conference_id() {
    let words = engine_core::calendar::ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
        dial_in_code: "Conference ID".into(),
    };
    let block = conference(&answer_with_numbers())
        .unwrap()
        .text_block(&words)
        .unwrap();
    assert!(
        block.contains("Dial in: +31 800 000 0000\nConference ID: 123456789\n"),
        "{block}"
    );
}
