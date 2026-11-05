//! [`TalkMeetings`](super::TalkMeetings) over its real reqwest path, against an in-process mock
//! HTTP server answering with the harness's captured bytes.

use engine_core::{calendar::ConferenceService, error::FailureClass};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use serde_json::{Value, json};

use super::*;
use crate::test_support::mock_server_capturing;

const KEY: &str = "k5x2q7m4n9b3v8c1z6l0p2w4r7";
const CAPABILITIES: &str = include_str!("../tests/fixtures/talk-capabilities.json");
const CREATED: &str = include_str!("../tests/fixtures/talk-room-created.json");

fn http(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The captured capabilities with `conversations.<name>` set to `value`.
fn capabilities_with(name: &str, value: bool) -> String {
    let mut body: Value = serde_json::from_str(CAPABILITIES).unwrap();
    body["ocs"]["data"]["capabilities"]["spreed"]["config"]["conversations"][name] = json!(value);
    body.to_string()
}

fn talk(server: &str) -> TalkMeetings {
    TalkMeetings::new(
        server,
        Credentials::Basic {
            username: "alice".into(),
            password: "secret".into(),
        },
        &TlsClientConfig::default(),
        &RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .unwrap()
}

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new(KEY).unwrap(),
        title: "Review".into(),
        start: "2026-09-14T09:00:00Z".parse().unwrap(),
        end: "2026-09-14T09:30:00Z".parse().unwrap(),
    }
}

/// The JSON body of a captured request.
fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn a_conversation_is_made_public_and_named_after_the_title() {
    let (server, seen) = mock_server_capturing(vec![
        http("200 OK", CAPABILITIES),
        http("201 Created", CREATED),
    ]);
    let talk = talk(&server);
    assert_eq!(talk.service(), ConferenceService::NextcloudTalk);
    let meeting = talk.create_meeting(&request()).await.unwrap();
    let link = format!("{server}/index.php/call/83h5s7dz");
    assert_eq!(meeting.key.as_str(), link);
    assert_eq!(meeting.service, Some(ConferenceService::NextcloudTalk));
    assert_eq!(meeting.entry_points[0].access.password, None);

    let seen = seen.lock().unwrap();
    assert!(
        seen[0].starts_with("GET /ocs/v2.php/cloud/capabilities "),
        "{}",
        seen[0]
    );
    let create = &seen[1];
    assert!(
        create.starts_with("POST /ocs/v2.php/apps/spreed/api/v4/room "),
        "{create}"
    );
    let head = create.to_ascii_lowercase();
    assert!(head.contains("ocs-apirequest: true"), "{create}");
    assert!(head.contains("authorization: basic"), "{create}");
    assert_eq!(body(create), json!({ "roomType": 3, "roomName": "Review" }));
}

#[tokio::test]
async fn a_server_that_forces_passwords_is_given_the_key() {
    let (server, seen) = mock_server_capturing(vec![
        http("200 OK", &capabilities_with("force-passwords", true)),
        http("201 Created", CREATED),
    ]);
    let meeting = talk(&server).create_meeting(&request()).await.unwrap();
    assert_eq!(
        meeting.entry_points[0].access.password.as_deref(),
        Some(KEY)
    );
    assert_eq!(body(&seen.lock().unwrap()[1])["password"], KEY);
}

#[tokio::test]
async fn an_account_that_may_not_make_one_is_refused_before_asking() {
    let (server, seen) = mock_server_capturing(vec![http(
        "200 OK",
        &capabilities_with("can-create", false),
    )]);
    let err = talk(&server).create_meeting(&request()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_server_without_talk_is_refused() {
    let without = json!({ "ocs": { "data": { "capabilities": { "core": {} } } } }).to_string();
    let (server, _) = mock_server_capturing(vec![http("200 OK", &without)]);
    let err = talk(&server).create_meeting(&request()).await.unwrap_err();
    assert!(err.detail().contains("no Talk"), "{err}");

    let (server, _) = mock_server_capturing(vec![
        http("200 OK", CAPABILITIES),
        http("404 Not Found", "{}"),
    ]);
    let err = talk(&server).create_meeting(&request()).await.unwrap_err();
    assert!(err.detail().contains("no Talk"), "{err}");
}

#[tokio::test]
async fn talk_is_offered_where_the_account_may_make_a_conversation() {
    let (server, seen) = mock_server_capturing(vec![http("200 OK", CAPABILITIES)]);
    assert!(talk(&server).offered().await.unwrap());
    assert!(
        seen.lock().unwrap()[0].starts_with("GET /ocs/v2.php/cloud/capabilities "),
        "only the capabilities are asked"
    );

    let (server, _) = mock_server_capturing(vec![http(
        "200 OK",
        &capabilities_with("can-create", false),
    )]);
    assert!(!talk(&server).offered().await.unwrap());

    let without = json!({ "ocs": { "data": { "capabilities": { "core": {} } } } }).to_string();
    let (server, _) = mock_server_capturing(vec![http("200 OK", &without)]);
    assert!(!talk(&server).offered().await.unwrap());
}

#[tokio::test]
async fn a_server_that_refuses_the_question_does_not_say_talk_is_missing() {
    let (server, _) = mock_server_capturing(vec![http("401 Unauthorized", "{}")]);
    assert!(talk(&server).offered().await.is_err());
}

#[tokio::test]
async fn a_refusal_or_an_answer_without_a_conversation_is_an_error() {
    let refused = r#"{"ocs":{"meta":{"statuscode":400},"data":{"error":"password"}}}"#;
    let (server, _) = mock_server_capturing(vec![
        http("200 OK", CAPABILITIES),
        http("400 Bad Request", refused),
    ]);
    let err = talk(&server).create_meeting(&request()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.detail().starts_with("Talk HTTP 400: "), "{err}");

    let (server, _) = mock_server_capturing(vec![
        http("200 OK", CAPABILITIES),
        http("201 Created", r#"{"ocs":{"data":{}}}"#),
    ]);
    assert!(talk(&server).create_meeting(&request()).await.is_err());

    let (server, _) = mock_server_capturing(vec![
        http("200 OK", CAPABILITIES),
        http("201 Created", "not json"),
    ]);
    assert!(talk(&server).create_meeting(&request()).await.is_err());
}

#[tokio::test]
async fn a_conversation_is_deleted_by_the_token_in_its_link() {
    let (server, seen) = mock_server_capturing(vec![
        http("200 OK", r#"{"ocs":{"data":[]}}"#),
        http("404 Not Found", "{}"),
    ]);
    let talk = talk(&server);
    let meeting = conference(&talk.link("83h5s7dz"), None).unwrap();
    talk.delete_meeting(&meeting).await.unwrap();
    talk.delete_meeting(&meeting).await.unwrap();
    let seen = seen.lock().unwrap();
    assert!(
        seen[0].starts_with("DELETE /ocs/v2.php/apps/spreed/api/v4/room/83h5s7dz "),
        "{}",
        seen[0]
    );
}

#[tokio::test]
async fn a_refused_delete_is_an_error() {
    let (server, _) = mock_server_capturing(vec![http("403 Forbidden", "{}")]);
    let talk = talk(&server);
    let meeting = conference(&talk.link("83h5s7dz"), None).unwrap();
    assert!(talk.delete_meeting(&meeting).await.is_err());
}

#[tokio::test]
async fn a_link_to_another_server_is_never_deleted_here() {
    let (server, seen) = mock_server_capturing(vec![]);
    let talk = talk(&server);
    let elsewhere = conference("https://other.example.org/index.php/call/83h5s7dz", None).unwrap();
    let err = talk.delete_meeting(&elsewhere).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_bearer_token_is_sent_as_one() {
    let (server, seen) = mock_server_capturing(vec![http("200 OK", "{}")]);
    let talk = TalkMeetings::new(
        &server,
        Credentials::Bearer("token".into()),
        &TlsClientConfig::default(),
        &RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .unwrap();
    let meeting = conference(&talk.link("83h5s7dz"), None).unwrap();
    talk.delete_meeting(&meeting).await.unwrap();
    let head = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.contains("authorization: bearer token"), "{head}");
}

#[tokio::test]
async fn a_throttle_it_does_not_wait_out_is_reported_with_its_wait() {
    let throttled = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (server, _) = mock_server_capturing(vec![throttled.to_owned()]);
    let talk = TalkMeetings::new(
        &server,
        Credentials::Bearer("token".into()),
        &TlsClientConfig::default(),
        &RetryConfig::default().with_policy(engine_http::RetryPolicy::none()),
        &engine_http::test_server::user_agent(),
    )
    .unwrap();
    let err = talk.create_meeting(&request()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::RateLimited);
    assert_eq!(
        err.retry_after(),
        Some(core::time::Duration::from_secs(30).into())
    );
    assert!(err.detail().starts_with("Talk HTTP 429"), "{err}");
}

#[tokio::test]
async fn a_server_that_does_not_answer_is_a_talk_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let err = talk(&server).create_meeting(&request()).await.unwrap_err();
    assert!(err.detail().starts_with("Talk transport error"), "{err}");
}
