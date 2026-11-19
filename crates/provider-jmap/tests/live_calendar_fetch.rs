//! Gated live calendar fetch against the Stalwart harness: the seeded calendars and events as
//! JMAP serves them, their meetings included. Skips with no `STALWART_HTTP_ADDR`, so the offline
//! suite stays green.

mod common;

use common::*;
use engine_core::calendar::{ConferenceOrigin, DetectedIn};
use engine_provider::Provider;

#[tokio::test]
async fn live_calendar_fetch() {
    let Some(provider) = setup("live_calendar_fetch").await else {
        return;
    };

    assert!(
        provider
            .sync_calendars(&account(), None)
            .await
            .unwrap()
            .sync
            .is_snapshot()
    );
    let objects = all_events(&provider).await;
    let uids: std::collections::BTreeSet<&str> = objects.iter().map(|e| e.uid.as_str()).collect();
    for uid in [
        "oneoff-2001@test.local",
        "weekly-2002@test.local",
        "meeting-2003@test.local",
        "virtual-2004@test.local",
        "allday-2005@test.local",
        "floating-2006@test.local",
        "work-2101@test.local",
        "work-2102@test.local",
    ] {
        assert!(uids.contains(uid), "seed event uid missing: {uid}");
    }
    // Stalwart serves the seed's three `CONFERENCE` lines as three `virtualLocations`, which
    // read back as one meeting with its two dial-in numbers. `virtualLocations` is a map, so
    // the numbers come in the order of the keys the server gave them, not the seed's.
    let conferences = &objects
        .iter()
        .find(|e| e.uid.as_str() == "virtual-2004@test.local")
        .unwrap()
        .conferences;
    assert_eq!(conferences.len(), 1, "{conferences:?}");
    let mut entry_uris: Vec<&str> = conferences[0]
        .entry_points
        .iter()
        .map(|entry| entry.uri.as_str())
        .collect();
    entry_uris.sort_unstable();
    assert_eq!(
        entry_uris,
        [
            "https://meet.example.com/harness-room",
            "tel:+15555550100",
            "tel:+15555550101,,123456#"
        ]
    );
    // JMAP carries no `X-GOOGLE-CONFERENCE`, so the Google-shaped invitation's meeting is the
    // one its description's block links to, and the notes leave the block out.
    let invitation = objects
        .iter()
        .find(|e| e.uid.as_str() == "meeting-2003@test.local")
        .unwrap();
    let found: Vec<_> = invitation
        .conferences
        .iter()
        .map(|c| (c.join().map(ToString::to_string), c.origin))
        .collect();
    assert_eq!(
        found,
        [(
            Some("https://meet.google.com/abc-defg-hij".to_owned()),
            ConferenceOrigin::Detected {
                place: DetectedIn::Description
            }
        )]
    );
    assert_eq!(
        invitation.notes(|_| true).as_deref(),
        Some("Bring the quarterly figures.")
    );
}
