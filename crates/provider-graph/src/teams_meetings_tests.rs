use engine_core::calendar::{ConferenceUri, EntryKind};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use serde_json::{Value, json};

use super::*;
use crate::{
    principal::MailboxPrincipal,
    test_support::{FakeRoute, fake_client_logged},
};

const KEY: &str = "k5x2q7m4n9b3v8c1z6l0p2w4r7";
const JOIN: &str = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0?context=%7b%22Tid%22%3a%22t%22%7d";

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new(KEY).unwrap(),
        title: "Review".into(),
        start: "2026-09-14T09:00:00Z".parse().unwrap(),
        end: "2026-09-14T09:30:00Z".parse().unwrap(),
    }
}

fn answer() -> Value {
    json!({
        "id": "MSo1N2Y5_meeting",
        "externalId": KEY,
        "joinWebUrl": JOIN,
        "joinMeetingIdSettings": {
            "isPasscodeRequired": true,
            "joinMeetingId": "36833578137039",
            "passcode": "vb3T5Ag6"
        },
        "audioConferencing": {
            "conferenceId": "123456789",
            "tollNumber": "+31 20 123 4567",
            "tollFreeNumbers": ["+31 800 000 0000", "+31 20 123 4567"]
        }
    })
}

fn service(routes: Vec<(&str, FakeRoute)>) -> (TeamsMeetings, crate::test_support::RequestLog) {
    let (client, _, requests) = fake_client_logged(routes);
    (TeamsMeetings::new(client), requests)
}

#[tokio::test]
async fn a_meeting_is_made_with_the_key_as_its_external_id() {
    let (teams, requests) = service(vec![("/me/onlineMeetings/createOrGet", Ok(answer()))]);
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
    let (teams, _) = service(vec![("createOrGet", Ok(answer()))]);
    let meeting = teams.create_meeting(&request()).await.unwrap();
    assert_eq!(meeting.key.as_str(), JOIN);
    assert_eq!(meeting.service, Some(ConferenceService::Teams));
    assert_eq!(meeting.origin, ConferenceOrigin::Attached);
    assert_eq!(meeting.label.as_deref(), Some("Microsoft Teams"));
    assert_eq!(meeting.meeting_id.as_deref(), Some("36833578137039"));
    let join = &meeting.entry_points[0];
    assert_eq!(join.uri.as_str(), JOIN);
    assert_eq!(join.access.passcode.as_deref(), Some("vb3T5Ag6"));
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
}

#[tokio::test]
async fn an_account_without_audio_conferencing_has_only_the_link() {
    let mut bare = answer();
    bare.as_object_mut().unwrap().remove("audioConferencing");
    bare["joinMeetingIdSettings"]["passcode"] = Value::Null;
    let (teams, _) = service(vec![("createOrGet", Ok(bare))]);
    let meeting = teams.create_meeting(&request()).await.unwrap();
    assert_eq!(meeting.entry_points.len(), 1);
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
    let (teams, requests) = service(vec![
        ("$filter=", Ok(json!({ "value": [answer()] }))),
        ("/me/onlineMeetings/MSo1N2Y5_meeting", Ok(Value::Null)),
    ]);
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
        "https://graph.test/me/onlineMeetings/MSo1N2Y5_meeting"
    );
}

#[tokio::test]
async fn a_meeting_read_back_from_an_event_is_deleted_by_its_link_alone() {
    let (teams, requests) = service(vec![
        ("$filter=", Ok(json!({ "value": [answer()] }))),
        ("/me/onlineMeetings/MSo1N2Y5_meeting", Ok(Value::Null)),
    ]);
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
    let gone = || Err((404, json!({ "error": { "code": "NotFound" } })));
    let meeting = conference(&answer()).unwrap();

    let (teams, requests) = service(vec![("$filter=", gone())]);
    teams.delete_meeting(&meeting).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (teams, requests) = service(vec![("$filter=", Ok(json!({ "value": [] })))]);
    teams.delete_meeting(&meeting).await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (teams, _) = service(vec![
        ("$filter=", Ok(json!({ "value": [answer()] }))),
        ("/me/onlineMeetings/MSo1N2Y5_meeting", gone()),
    ]);
    teams.delete_meeting(&meeting).await.unwrap();
}

#[tokio::test]
async fn a_refused_delete_is_an_error() {
    let refused = || Err((403, json!({ "error": { "code": "Forbidden" } })));
    let meeting = conference(&answer()).unwrap();
    let (teams, _) = service(vec![("$filter=", refused())]);
    assert!(teams.delete_meeting(&meeting).await.is_err());
    let (teams, _) = service(vec![
        ("$filter=", Ok(json!({ "value": [answer()] }))),
        ("/me/onlineMeetings/MSo1N2Y5_meeting", refused()),
    ]);
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
