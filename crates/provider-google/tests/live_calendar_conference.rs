//! Gated live check that a Meet conference Google makes reads back as the calendar's own.
//!
//! The event is created by direct HTTP with a `conferenceData.createRequest` under
//! `conferenceDataVersion=1`, because asking the server for a meeting is not something the
//! adapter writes yet; it is the fixture. Reading it back goes through the adapter's own sync,
//! which is what a host sees. Google makes the conference asynchronously, so the read is
//! repeated until the request is no longer pending.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_calendar_conference -- --nocapture
//! ```

mod common;

use common::*;
use engine_core::{
    calendar::{Conference, ConferenceOrigin, ConferenceService, NativeState},
    sync::SyncUpdate,
};
use engine_provider::Provider;

const EVENTS: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";

/// An HTTP client on the engine's own TLS policy (a bare `reqwest::Client` has no rustls
/// crypto provider in this workspace).
fn http() -> reqwest::Client {
    engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("an HTTP client on the engine's TLS policy")
}

#[tokio::test]
async fn live_a_meet_conference_reads_back_as_the_calendars_own() {
    let Some(token) = token() else {
        eprintln!("skipping live_a_meet_conference_reads_back...: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(14);
    let title = format!("PIM live conference {stamp}");
    let created: serde_json::Value = http()
        .post(format!("{EVENTS}?conferenceDataVersion=1&sendUpdates=none"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "summary": title,
            "start": { "dateTime": format!("{date}T10:00:00"), "timeZone": "Europe/Amsterdam" },
            "end": { "dateTime": format!("{date}T10:30:00"), "timeZone": "Europe/Amsterdam" },
            "conferenceData": { "createRequest": {
                "requestId": format!("pim-live-{stamp}"),
                "conferenceSolutionKey": { "type": "hangoutsMeet" }
            }}
        }))
        .send()
        .await
        .expect("POST events")
        .error_for_status()
        .expect("Google takes the request for a Meet conference")
        .json()
        .await
        .expect("created json");
    let id = created["id"].as_str().expect("an event id").to_owned();

    let provider = calendar_provider(token.clone());
    let mut conferences = Vec::new();
    for _ in 0..10 {
        conferences = read_back(&provider, &title).await;
        let pending = conferences.iter().any(|c| {
            matches!(
                c.origin,
                ConferenceOrigin::Native {
                    state: NativeState::Pending,
                    ..
                }
            )
        });
        if !pending {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    http()
        .delete(format!("{EVENTS}/{id}?sendUpdates=none"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("DELETE the fixture event");

    eprintln!("read back: {conferences:#?}");
    assert_eq!(conferences.len(), 1);
    let meet = &conferences[0];
    assert_eq!(meet.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(
        meet.origin,
        ConferenceOrigin::Native {
            removable: true,
            state: NativeState::Ready
        }
    );
    let join = meet.join().expect("a join link");
    assert_eq!(join.host().as_deref(), Some("meet.google.com"));
    assert_eq!(
        ConferenceService::recognise(join),
        Some(ConferenceService::GoogleMeet)
    );
    assert!(meet.meeting_id.is_some(), "Google names the conference");
}

/// The conferences of the event titled `title`, as the adapter's sync reads them.
async fn read_back(
    provider: &provider_google::GoogleCalendarProvider,
    title: &str,
) -> Vec<Conference> {
    let events = provider
        .sync_events(&account(), &primary(provider).await, None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
        panic!("expected an event snapshot");
    };
    objects
        .into_iter()
        .find(|event| event.title == title)
        .expect("the created event syncs")
        .conferences
}
