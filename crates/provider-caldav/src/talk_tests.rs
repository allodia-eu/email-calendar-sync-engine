use serde_json::json;

use super::*;

const SERVER: &str = "https://cloud.example.org";
/// The harness's capabilities, captured with Talk installed and nothing configured.
const CAPABILITIES: &str = include_str!("../tests/fixtures/talk-capabilities.json");
/// The harness's answer to making a public conversation.
const CREATED: &str = include_str!("../tests/fixtures/talk-room-created.json");

fn ocs_data(fixture: &str) -> Value {
    let body: Value = serde_json::from_str(fixture).unwrap();
    body["ocs"]["data"].clone()
}

#[test]
fn a_server_as_installed_lets_anyone_make_a_conversation_without_a_password() {
    assert_eq!(
        rules(&ocs_data(CAPABILITIES)),
        Some(Rules {
            can_create: true,
            force_passwords: false
        })
    );
}

#[test]
fn a_new_conversation_is_named_by_its_token() {
    assert_eq!(created(&ocs_data(CREATED)), Some("83h5s7dz"));
    assert_eq!(created(&json!({ "token": "../x" })), None);
    assert_eq!(created(&json!({})), None);
}

#[test]
fn the_capabilities_say_whether_a_conversation_may_be_made_and_needs_a_password() {
    let data = |conversations: Value| json!({ "capabilities": { "spreed": { "config": { "conversations": conversations } } } });
    assert_eq!(
        rules(&data(
            json!({ "can-create": true, "force-passwords": true })
        )),
        Some(Rules {
            can_create: true,
            force_passwords: true
        })
    );
    assert_eq!(
        rules(&data(json!({ "can-create": false }))),
        Some(Rules {
            can_create: false,
            force_passwords: false
        })
    );
    assert_eq!(
        rules(&data(json!({}))),
        Some(Rules {
            can_create: true,
            force_passwords: false
        })
    );
    assert_eq!(rules(&json!({ "capabilities": { "core": {} } })), None);
    assert_eq!(rules(&Value::Null), None);
}

#[test]
fn a_name_is_cut_to_what_talk_accepts_at_a_character() {
    assert_eq!(name("  Review  "), "Review");
    let long = "é".repeat(300);
    assert_eq!(name(&long).chars().count(), NAME_LIMIT);
}

#[test]
fn a_token_is_read_from_a_link_to_this_server_only() {
    assert_eq!(
        token_in(SERVER, "https://cloud.example.org/index.php/call/abc123xy"),
        Some("abc123xy")
    );
    assert_eq!(
        token_in(SERVER, "https://cloud.example.org/call/abc123xy/"),
        Some("abc123xy")
    );
    assert_eq!(
        token_in(SERVER, "https://cloud.example.org/call/abc123xy?x=1#y"),
        Some("abc123xy")
    );
    assert_eq!(
        token_in(
            "https://example.org/nextcloud",
            "https://example.org/nextcloud/index.php/call/abc123xy"
        ),
        Some("abc123xy")
    );
    for other in [
        "https://cloud.example.org.evil.test/call/abc123xy",
        "https://other.example.org/call/abc123xy",
        "https://cloud.example.org/apps/spreed/abc123xy",
        "https://cloud.example.org/call/",
        "https://cloud.example.org/call/abc%2F..",
    ] {
        assert_eq!(token_in(SERVER, other), None, "{other}");
    }
    assert_eq!(
        token_in(
            "https://example.org/nc",
            "https://example.org/nc2/call/abc123xy"
        ),
        None
    );
}

#[test]
fn a_conversation_is_a_meeting_made_elsewhere_with_its_password() {
    let link = format!("{SERVER}/index.php/call/abc123xy");
    let meeting = conference(&link, Some("k5x2q7m4n9b3v8c1z6l0p2w4r7")).unwrap();
    assert_eq!(meeting.key.as_str(), link);
    assert_eq!(meeting.service, Some(ConferenceService::NextcloudTalk));
    assert_eq!(meeting.origin, ConferenceOrigin::Attached);
    assert_eq!(meeting.label.as_deref(), Some("Nextcloud Talk"));
    assert_eq!(
        meeting.join().map(ConferenceUri::as_str),
        Some(link.as_str())
    );
    assert_eq!(
        meeting.entry_points[0].access.password.as_deref(),
        Some("k5x2q7m4n9b3v8c1z6l0p2w4r7")
    );
    assert_eq!(
        conference(&link, None).unwrap().entry_points[0]
            .access
            .password,
        None
    );
}

#[test]
fn a_server_is_an_http_address_without_a_query() {
    let agent = engine_http::test_server::user_agent();
    let make = |server: &str| {
        TalkMeetings::new(
            server,
            Credentials::Bearer("t".into()),
            &TlsClientConfig::bundled(),
            &RetryConfig::default(),
            &agent,
        )
    };
    assert_eq!(make("https://cloud.example.org/").unwrap().server, SERVER);
    assert_eq!(
        make("https://example.org/nextcloud/").unwrap().server,
        "https://example.org/nextcloud"
    );
    assert!(make("ftp://cloud.example.org").is_err());
    assert!(make("https://cloud.example.org/?x=1").is_err());
    assert!(make("https://cloud.example.org/#apps").is_err());
    assert!(make("https://alice@cloud.example.org").is_err());
    assert!(make("https://alice:pw@cloud.example.org").is_err());
    assert!(make("not a url").is_err());
    let talk = make(SERVER).unwrap();
    assert_eq!(
        talk.link("abc123xy"),
        format!("{SERVER}/index.php/call/abc123xy")
    );
    assert!(!format!("{talk:?}").contains("\"t\""));
}
