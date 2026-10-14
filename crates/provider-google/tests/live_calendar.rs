//! Gated live integration for **Google Calendar**: the calendar list, the snapshot/delta
//! cycle, and the create/patch/delete write path against a throwaway account.
//!
//! Split from the Gmail suite in `live_provider.rs` — same account and the same
//! `GOOGLE_ACCESS_TOKEN` gate, different product surface.

mod common;

use common::*;
use engine_core::{ids::CalendarId, sync::SyncUpdate};
use engine_provider::{CalendarWrites, Provider};
#[tokio::test]
async fn live_calendars_list() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendars_list: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let sync = calendar_provider(token)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    assert!(sync.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &sync.update else {
        panic!("expected a calendar snapshot");
    };
    // The account has at least its own primary calendar.
    assert!(objects.iter().any(|c| c.is_default), "a primary calendar");
}

#[tokio::test]
async fn live_calendar_snapshot_then_delta_cycle() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_snapshot_then_delta_cycle: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    // A first sync is a reconciling snapshot; capture the per-calendar syncToken.
    let snapshot = provider
        .sync_events(&account(), None)
        .await
        .expect("snapshot");
    assert!(snapshot.is_snapshot());
    let cursor = snapshot.next_cursor.clone();
    assert!(!cursor.as_str().is_empty());

    // An immediate delta from that syncToken must not error and must not be a snapshot —
    // proving the real events.list?syncToken request shape is accepted.
    let delta = provider
        .sync_events(&account(), Some(&cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot(), "a delta from a live syncToken");
    assert!(!delta.next_cursor.as_str().is_empty());
}

#[tokio::test]
async fn live_calendar_create_patch_delete() {
    use engine_core::{
        calendar::ParticipantRole,
        ids::Uid,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    };
    use engine_provider::{
        CalendarAddress, EventDeletion, EventDraft, EventEdit, EventPatch, Invitee, InviteePatch,
        MeetingDraft, PatchTarget, SchedulingIdentity,
    };

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_create_patch_delete: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let calendars = provider
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let organiser = objects
        .into_iter()
        .find(|calendar| calendar.is_default)
        .expect("a primary calendar")
        .id;
    let organiser = CalendarAddress::parse(organiser.key().as_str())
        .expect("the primary calendar id is the account's email address");
    let invitee = CalendarAddress::parse("calendar-engine-live@example.invalid").unwrap();
    let stamp: UtcDateTime = "2026-07-18T10:00:00Z".parse().unwrap();
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(60);
    let at = |hour: u8| {
        format!("{date}T{hour:02}:00:00")
            .parse::<LocalDateTime>()
            .unwrap()
    };
    // Create a throwaway event.
    let draft = EventDraft::new(
        cal.clone(),
        Uid::new(format!("live-cal-{}@example.test", std::process::id())).unwrap(),
        "Live invitation create/patch/delete",
        CalendarDateTime::Zoned {
            local: at(10),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
        CalendarDateTime::Zoned {
            local: at(11),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
        stamp,
    )
    .location("Room Live")
    .meeting(MeetingDraft::new(
        SchedulingIdentity::new(organiser),
        vec![Invitee::required(SchedulingIdentity::new(invitee.clone()))],
    ));
    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create");
    let first_etag = created.revisions.etag.clone();
    assert!(first_etag.is_some(), "the created event carries an ETag");

    let snapshot = provider
        .sync_events(&account(), None)
        .await
        .expect("read create");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        panic!("expected an event snapshot");
    };
    let base = objects
        .into_iter()
        .find(|event| event.id == created.event)
        .expect("the created invitation reads back");
    assert!(base.participants.iter().any(|participant| {
        participant.email.as_deref() == Some(invitee.as_str())
            && participant.has_role(&ParticipantRole::Attendee)
    }));
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp)
            .summary("Live invitation create/patch/delete (renamed)")
            .invitees(
                InviteePatch::new()
                    .upsert(Invitee::optional(SchedulingIdentity::new(invitee.clone()))),
            ),
    );
    let patched = provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch");
    assert!(
        patched.revisions.etag.is_some() && patched.revisions.etag != first_etag,
        "the ETag advances on patch"
    );

    let snapshot = provider
        .sync_events(&account(), None)
        .await
        .expect("read patch");
    let SyncUpdate::Snapshot { objects, .. } = snapshot.update else {
        panic!("expected an event snapshot");
    };
    let base = objects
        .into_iter()
        .find(|event| event.id == created.event)
        .expect("the patched invitation reads back");
    assert!(base.participants.iter().any(|participant| {
        participant.email.as_deref() == Some(invitee.as_str())
            && participant.has_role(&ParticipantRole::Optional)
    }));

    // Delete it, guarded by the fresh ETag. Google sends the cancellation.
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete");
    // NOTE: a *guarded* re-delete here returns 412 (conditionNotMet), not 404/410 — the
    // deleted event is left cancelled with a new ETag, so the stale If-Match fails the
    // precondition (a real Google finding; see tests/fixtures/README.md). The
    // 404/410-gone idempotency is covered offline (`cal_write` tests).
}
