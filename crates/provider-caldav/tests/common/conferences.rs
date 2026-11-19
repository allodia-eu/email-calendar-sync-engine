//! The meetings the shared calendar seed holds, as every DAV server's copy must read.

use engine_core::calendar::{ConferenceOrigin, ConferenceService, DetectedIn, Event};

/// Asserts the seed's two meetings among `events`: `virtual-2004`'s `CONFERENCE` join link
/// with its two dial-in numbers as one conference, the second with a PIN after its pause, and
/// `meeting-2003`, shaped as a Google invitation, as the meeting its `X-GOOGLE-CONFERENCE`
/// names, with notes that leave out Google's block.
pub(crate) fn the_seeds_meetings_read_as_one_conference_each(events: &[Event]) {
    let by_uid = |uid: &str| events.iter().find(|e| e.uid.as_str() == uid).unwrap();

    let conferences = &by_uid("virtual-2004@test.local").conferences;
    assert_eq!(
        conferences.len(),
        1,
        "the dial-in joins the meeting: {conferences:?}"
    );
    let entry_uris: Vec<&str> = conferences[0]
        .entry_points
        .iter()
        .map(|entry| entry.uri.as_str())
        .collect();
    assert_eq!(
        entry_uris,
        [
            "https://meet.example.com/harness-room",
            "tel:+15555550100",
            "tel:+15555550101,,123456#"
        ]
    );

    let invitation = by_uid("meeting-2003@test.local");
    let found: Vec<_> = invitation
        .conferences
        .iter()
        .map(|c| {
            (
                c.join().map(ToString::to_string),
                c.service.clone(),
                c.origin,
            )
        })
        .collect();
    assert_eq!(
        found,
        [(
            Some("https://meet.google.com/abc-defg-hij".to_owned()),
            Some(ConferenceService::GoogleMeet),
            ConferenceOrigin::Detected {
                place: DetectedIn::VendorProperty
            }
        )],
        "the server keeps the vendor property, and the block's link is the same meeting"
    );
    assert_eq!(
        invitation.notes(|_| true).as_deref(),
        Some("Bring the quarterly figures.")
    );
}
