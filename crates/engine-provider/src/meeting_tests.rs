use engine_core::calendar::{ConferenceOrigin, ConferenceUri};

use super::*;

const KEY: &str = "k5x2q7m4n9b3v8c1z6l0p2w4r7";

fn request() -> MeetingRequest {
    MeetingRequest {
        key: MeetingKey::new(KEY).unwrap(),
        title: "Review".into(),
        start: "2026-09-14T09:00:00Z".parse().unwrap(),
        end: "2026-09-14T09:30:00Z".parse().unwrap(),
    }
}

#[test]
fn a_key_is_long_enough_for_128_bits_and_lowercase() {
    assert!(MeetingKey::new(KEY).is_ok());
    assert!(MeetingKey::new("a".repeat(64)).is_ok());
    assert_eq!(MeetingKey::new("a".repeat(25)), Err(MeetingKeyError));
    assert_eq!(MeetingKey::new("a".repeat(65)), Err(MeetingKeyError));
    assert_eq!(
        MeetingKey::new(KEY.to_ascii_uppercase()),
        Err(MeetingKeyError)
    );
    assert_eq!(MeetingKey::new(format!("{KEY}/x")), Err(MeetingKeyError));
}

#[test]
fn a_key_refused_by_its_shape_is_refused_when_read_back() {
    let json = serde_json::to_string(&request()).unwrap();
    let back: MeetingRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back, request());
    let tampered = json.replace(KEY, "short");
    assert!(serde_json::from_str::<MeetingRequest>(&tampered).is_err());
}

#[tokio::test]
async fn a_jitsi_meeting_is_a_room_on_the_server_named_by_the_key() {
    let jitsi = JitsiMeetings::new("https://meet.example.org/").unwrap();
    assert_eq!(jitsi.server(), "https://meet.example.org");
    let meeting = jitsi.create_meeting(&request()).await.unwrap();
    let link = format!("https://meet.example.org/{KEY}");
    assert_eq!(
        meeting.join().map(ConferenceUri::as_str),
        Some(link.as_str())
    );
    assert_eq!(meeting.key.as_str(), link);
    assert_eq!(meeting.service, Some(ConferenceService::Jitsi));
    assert_eq!(meeting.origin, ConferenceOrigin::Attached);
    assert_eq!(meeting.label.as_deref(), Some("Jitsi Meet"));
    assert!(jitsi.delete_meeting(&meeting).await.is_ok());
}

#[tokio::test]
async fn a_jitsi_server_beneath_a_path_keeps_it() {
    let jitsi = JitsiMeetings::new("https://example.org/jitsi").unwrap();
    let meeting = jitsi.create_meeting(&request()).await.unwrap();
    assert_eq!(
        meeting.join().map(ConferenceUri::as_str),
        Some(format!("https://example.org/jitsi/{KEY}").as_str())
    );
}

#[test]
fn a_jitsi_server_is_an_https_address_a_room_can_follow() {
    for refused in [
        "http://meet.example.org",
        "meet.example.org",
        "https://meet.example.org/?config=x",
        "https://meet.example.org/#room",
        "tel:+31201234567",
    ] {
        assert_eq!(
            JitsiMeetings::new(refused),
            Err(JitsiServerError),
            "{refused}"
        );
    }
}

#[test]
fn a_key_is_hidden_from_debug_output() {
    let shown = format!("{:?}", request());
    assert!(!shown.contains(KEY), "{shown}");
    assert!(shown.contains("MeetingKey(\"<redacted>\")"), "{shown}");
}

#[test]
fn a_refusal_says_what_a_key_and_a_server_must_be() {
    assert_eq!(
        MeetingKeyError.to_string(),
        "a meeting key is 26 to 64 lowercase letters and digits"
    );
    assert_eq!(
        JitsiServerError.to_string(),
        "a Jitsi server is an https address without a query or fragment"
    );
}

#[test]
fn jitsi_names_its_service() {
    let jitsi = JitsiMeetings::new("https://meet.example.org").unwrap();
    assert_eq!(jitsi.service(), ConferenceService::Jitsi);
}
