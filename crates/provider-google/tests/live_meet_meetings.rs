//! Gated live check that `MeetMeetings` makes a Meet space a person can join. Its own token,
//! because the scope (`meetings.space.created`) is one no other suite needs:
//!
//! ```sh
//! GOOGLE_MEET_ACCESS_TOKEN=<token> \
//!   cargo test -p provider-google --test live_meet_meetings -- --nocapture
//! ```

use engine_core::calendar::{ConferenceOrigin, ConferenceService};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use provider_google::{GoogleClient, MeetMeetings};

#[tokio::test]
async fn live_a_space_is_made_with_a_join_link() {
    let Some(token) = std::env::var("GOOGLE_MEET_ACCESS_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
    else {
        eprintln!("skipping live_a_space_is_made_with_a_join_link: GOOGLE_MEET_ACCESS_TOKEN unset");
        return;
    };
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let meet = MeetMeetings::new(client);
    let request = MeetingRequest {
        key: MeetingKey::new("pimlivemeet0000000000000000").unwrap(),
        title: "PIM live Meet meeting".into(),
        start: "2026-12-14T09:00:00Z".parse().unwrap(),
        end: "2026-12-14T09:30:00Z".parse().unwrap(),
    };

    let made = meet.create_meeting(&request).await.expect("create");
    let join = made.join().expect("a join link");
    assert!(join.as_str().starts_with("https://meet.google.com/"));
    assert_eq!(made.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(made.origin, ConferenceOrigin::Attached);
    assert!(made.meeting_id.is_some());
    meet.delete_meeting(&made).await.expect("delete");
}
