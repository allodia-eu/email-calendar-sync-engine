//! Gated live check that `TeamsMeetings` makes a Teams meeting, answers a repeated key with
//! the same one, and deletes it so that it is gone. Its own token, because only a work or
//! school account can grant `OnlineMeetings.ReadWrite`:
//!
//! ```sh
//! GRAPH_MEETINGS_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- \
//!   --profile <work-profile> token)" \
//!   cargo test -p provider-graph --test live_teams_meetings -- --nocapture
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::calendar::{
    ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryKind,
};
use engine_provider::{MeetingKey, MeetingRequest, MeetingService};
use provider_graph::{GraphClient, TeamsMeetings};

fn token() -> Option<String> {
    std::env::var("GRAPH_MEETINGS_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn teams(token: &str) -> TeamsMeetings {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    TeamsMeetings::new(client)
}

/// A key no earlier run used: the clock in base 36, padded to the shortest key there is.
fn fresh_key() -> MeetingKey {
    let mut n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut key = String::from("pimlive");
    while n > 0 {
        key.push(char::from_digit(u32::try_from(n % 36).unwrap(), 36).unwrap());
        n /= 36;
    }
    while key.len() < 26 {
        key.push('0');
    }
    MeetingKey::new(key).unwrap()
}

#[tokio::test]
async fn live_a_meeting_is_made_once_per_key_and_deleted() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_meeting_is_made_once_per_key...: GRAPH_MEETINGS_ACCESS_TOKEN unset"
        );
        return;
    };
    let teams = teams(&token);
    let request = MeetingRequest {
        key: fresh_key(),
        title: "PIM live Teams meeting".into(),
        start: "2026-12-14T09:00:00Z".parse().unwrap(),
        end: "2026-12-14T09:30:00Z".parse().unwrap(),
    };

    let made = teams.create_meeting(&request).await.expect("create");
    let join = made.join().expect("a join link").clone();
    assert_eq!(made.service, Some(ConferenceService::Teams));
    assert_eq!(made.origin, ConferenceOrigin::Attached);
    assert_eq!(
        join.host().as_deref(),
        Some("teams.microsoft.com"),
        "{join:?}"
    );
    assert!(made.meeting_id.is_some(), "a meeting ID: {made:?}");
    assert_eq!(made.entry_points[0].kind, EntryKind::Video);

    let again = teams.create_meeting(&request).await.expect("create again");
    assert_eq!(
        again.join(),
        Some(&join),
        "a repeated key is the same meeting"
    );

    // Deleted as an event holds it after a round trip: only its link survives.
    let mut read_back = again.clone();
    read_back.key = ConferenceKey::new("meeting-1");
    read_back.entry_points.truncate(1);
    read_back.meeting_id = None;
    teams.delete_meeting(&read_back).await.expect("delete");
    teams
        .delete_meeting(&read_back)
        .await
        .expect("deleting a meeting already gone");

    // Graph makes a new meeting for a key whose meeting is gone, so a new link proves the
    // delete reached it.
    let after = teams
        .create_meeting(&request)
        .await
        .expect("create after delete");
    let after_join: &ConferenceUri = after.join().expect("a join link");
    assert_ne!(after_join, &join, "the deleted meeting is gone");
    teams.delete_meeting(&after).await.expect("clean up");
}
