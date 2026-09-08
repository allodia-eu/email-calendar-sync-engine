//! Meeting storage on a CalDAV server without RFC 6638 scheduling.

use engine_core::{
    calendar::{ParticipantRole, ParticipationStatus},
    ids::{AccountId, Uid},
    time::{CalendarDateTime, UtcDateTime},
};
use engine_provider::{
    CalendarAddress, CalendarWrites, EventDeletion, EventDraft, EventEdit, EventPatch, Invitee,
    InviteePatch, MeetingDraft, PatchTarget, SchedulingIdentity,
};
use provider_caldav::CalDavProvider;

use crate::common::{lines_without, pre_clean, require, server_ical};

const UID: &str = "caldav-meeting-roundtrip@test.local";

fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 9, 7, 10, 0, 0).unwrap()
}

fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(format!("2026-10-15T{hour:02}:00:00").parse().unwrap())
}

/// Creates and edits a meeting on a calendar-access-only server.
///
/// This proves that a server accepts and preserves the RFC 5545 organiser and attendee
/// properties emitted by the neutral API. It deliberately proves no delivery: without
/// RFC 6638 scheduling, the caller must send iTIP messages by another route.
pub(crate) async fn storage_round_trip(provider: &CalDavProvider, account: &AccountId) {
    let uid = Uid::new(UID).unwrap();
    pre_clean(provider, account, &uid).await;

    let organiser = CalendarAddress::parse("alice@test.local").unwrap();
    let guest = CalendarAddress::parse("guest@example.invalid").unwrap();
    provider
        .create_event(
            account,
            &EventDraft::new(
                provider.calendar_id(),
                uid,
                "CalDAV meeting storage",
                at(10),
                at(11),
                stamp(),
            )
            .meeting(MeetingDraft::new(
                SchedulingIdentity::named(organiser, "Alice"),
                vec![Invitee::required(SchedulingIdentity::named(
                    guest.clone(),
                    "Guest",
                ))],
            )),
        )
        .await
        .expect("create meeting");

    let created = require(provider, account, UID).await;
    let attendee = created
        .participants
        .iter()
        .find(|participant| participant.email.as_deref() == Some(guest.as_str()))
        .expect("guest is projected from ATTENDEE");
    assert!(attendee.has_role(&ParticipantRole::Attendee));
    assert_eq!(
        attendee.participation_status,
        ParticipationStatus::NeedsAction
    );
    assert!(attendee.expect_reply);

    let raw = server_ical(&created);
    assert!(
        raw.as_str()
            .contains("ORGANIZER;CN=Alice:mailto:alice@test.local")
    );
    assert!(raw.as_str().contains("ROLE=REQ-PARTICIPANT"));
    assert!(!raw.as_str().contains("METHOD:"));

    provider
        .patch_event(
            account,
            &created,
            &EventEdit::new(
                &created,
                PatchTarget::Series,
                EventPatch::new(stamp()).invitees(
                    InviteePatch::new()
                        .upsert(Invitee::optional(SchedulingIdentity::new(guest.clone()))),
                ),
            ),
        )
        .await
        .expect("make guest optional");

    let edited = require(provider, account, UID).await;
    let raw = server_ical(&edited);
    let unfolded = lines_without(raw.as_str(), &[]).join("\n");
    assert!(unfolded.contains("CN=Guest"), "{unfolded}");
    assert!(unfolded.contains("ROLE=OPT-PARTICIPANT"), "{unfolded}");
    assert!(unfolded.contains("PARTSTAT=NEEDS-ACTION"), "{unfolded}");
    assert!(!unfolded.contains("METHOD:"));

    provider
        .delete_event(account, None, &EventDeletion::of(&edited))
        .await
        .expect("delete meeting");
}
