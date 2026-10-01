//! Gated live integration for **Microsoft Graph calendars**: the calendar list, the
//! sync cycle, and the write path against a throwaway account.
//!
//! Split from the mail suite in `live_provider.rs` — same account and the same
//! `GRAPH_ACCESS_TOKEN` gate, different product surface.

mod common;

use common::*;
use engine_core::{
    calendar::ParticipantRole,
    ids::{CalendarId, Uid},
    sync::SyncUpdate,
    time::CalendarDateTime,
};
use engine_provider::{
    CalendarAddress, CalendarWrites, EventDeletion, EventDraft, EventEdit, EventPatch, Invitee,
    InviteePatch, MeetingDraft, PatchTarget, Provider, SchedulingIdentity,
};

#[tokio::test]
async fn live_calendar_lists_syncs_and_writes() {
    let Some(token) = token() else {
        eprintln!("skipping live_calendar_lists_syncs_and_writes: GRAPH_ACCESS_TOKEN unset");
        return;
    };

    // List calendars and find the default (the binding used for events + writes).
    let placeholder = CalendarId::try_from("placeholder").unwrap();
    let calendars = calendar_provider(&token, placeholder)
        .sync_calendars(&account(), None)
        .await
        .expect("sync calendars");
    let SyncUpdate::Snapshot { objects, .. } = &calendars.update else {
        panic!("expected a calendar snapshot");
    };
    let default = objects
        .iter()
        .find(|c| c.is_default)
        .expect("a default calendar");
    let calendar_id = default.id.clone();
    let organiser = CalendarAddress::parse(
        default
            .owner
            .as_deref()
            .expect("the default calendar has an owner address"),
    )
    .expect("the default calendar owner is an email address");
    let invitee = CalendarAddress::parse("calendar-engine-live@example.invalid").unwrap();
    let date = time::OffsetDateTime::now_utc().date() + time::Duration::days(60);

    let provider = calendar_provider_around(&token, calendar_id.clone(), date);

    // A snapshot of the calendar's events: masters + singles, each zoned in the display
    // zone (proving the Prefer: outlook.timezone request), recurrence mapped for a series.
    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    assert!(events.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    assert!(
        objects.iter().all(|e| matches!(
            e.start,
            CalendarDateTime::Zoned { .. } | CalendarDateTime::Date(_)
        )),
        "every event is zoned or all-day (never a bare UTC instant)"
    );
    // A delta from the fresh cursor is a delta, not a snapshot.
    let delta = provider
        .sync_events(&account(), Some(&events.next_cursor))
        .await
        .expect("delta");
    assert!(!delta.is_snapshot());

    // Create, change the roster, then delete a throwaway meeting. Each write uses the
    // returned ETag and asks Graph to deliver the corresponding scheduling message.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let uid = Uid::new(format!("live-cal-{unique}@allodia-e2e.test")).unwrap();
    let draft = EventDraft::new(
        calendar_id.clone(),
        uid.clone(),
        "provider-graph live invitation probe",
        zoned(&format!("{date}T10:00:00")),
        zoned(&format!("{date}T10:30:00")),
        "2026-07-18T10:00:00Z".parse().unwrap(),
    )
    .location("Room Z")
    .description("safe to delete")
    .meeting(MeetingDraft::new(
        SchedulingIdentity::new(organiser),
        vec![Invitee::required(SchedulingIdentity::new(invitee.clone()))],
    ));

    let created = provider
        .create_event(&account(), &draft)
        .await
        .expect("create_event");
    assert!(
        created.revisions.etag.is_some(),
        "Graph returns an ETag on create"
    );

    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("read create");
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
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
        EventPatch::new("2026-07-18T10:05:00Z".parse().unwrap())
            .summary("live invitation probe (renamed)")
            .invitees(
                InviteePatch::new()
                    .upsert(Invitee::optional(SchedulingIdentity::new(invitee.clone()))),
            ),
    );
    let patched = provider
        .patch_event(&account(), &base, &edit)
        .await
        .expect("patch_event");
    assert!(patched.revisions.etag.is_some());
    assert_ne!(
        patched.revisions.etag, created.revisions.etag,
        "a patch advances the ETag"
    );

    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("read patch");
    let SyncUpdate::Snapshot { objects, .. } = events.update else {
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

    // Delete it, guarded by the patch's ETag. Graph sends the cancellation.
    provider
        .delete_event(&account(), None, &EventDeletion::of(&base))
        .await
        .expect("delete_event");
    // (A repeat delete of the just-deleted event is NOT retried here: Graph answers a
    // re-delete with `400 ErrorInvalidRequest` — the item has moved to Deleted Items — not
    // the clean `404` the idempotent path keys on. The 404 idempotency is offline-tested;
    // the outbox's NeedsConfirmation path covers the genuinely-ambiguous retry.)
}
