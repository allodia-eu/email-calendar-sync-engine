//! Offline `CalendarEvent/set` tests: **create**, **destroy**, and the capability the adapter
//! advertises. The PatchObject an *update* produces is `calendar_patch_tests`.
//!
//! The fake executor serves its canned reply **whatever it is sent**, so a test that only
//! checked the returned receipt would pass with a completely malformed request
//! (`AGENTS.md`). Every test here therefore asserts the **request** the adapter produced —
//! the method name, the `using` set, and the exact JSON of the arguments. What no offline
//! test can prove is that a real server *accepts* it; that is `tests/live_calendar_write.rs`.

use engine_core::{
    calendar::Event,
    error::FailureClass,
    ids::EventId,
    time::{CalendarDate, CalendarDateTime},
};
use engine_provider::{
    Capabilities, EventDeletion, EventDraft, EventRsvp, EventWrite, RsvpResponse, WriteGuard,
};
use serde_json::{Value, json};

use super::{calendar_write_support::*, provider_test_support::*, *};
use crate::calendar_rsvp::my_participant_id;

#[tokio::test]
async fn create_posts_a_jscalendar_object_and_learns_the_server_assigned_id() {
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    )
    .description("agenda");

    let receipt = p.create_event(&account(), &draft).await.unwrap();

    // The id is the *server's*. Nothing else in the exchange reveals it, which is why the
    // receipt carries it — and why a caller must never mint an EventId for a create.
    assert_eq!(receipt.event, EventId::try_from(EVENT).unwrap());
    assert_eq!(receipt.uid, uid());
    // A JMAP object has no revision token, so there is nothing to report.
    assert!(receipt.revisions.is_empty());

    let (using, method, args) = exec.sole_call();
    assert_eq!(method, "CalendarEvent/set");
    assert!(using.iter().any(|u| u == "urn:ietf:params:jmap:calendars"));
    assert_eq!(
        args,
        json!({
            "accountId": "c",
            "create": { "new": {
                "@type": "Event",
                "uid": "evt-1@test.local",
                "calendarIds": { "b": true },
                "title": "Sprint planning",
                "description": "agenda",
                "start": "2026-08-01T09:00:00",
                "timeZone": "Europe/Amsterdam",
                "duration": "PT30M",
            }}
        }),
        "the create must post a JSCalendar object with the wall clock and the zone stated \
         separately — never the UTC instant"
    );
    // And no `ifInState`: it guards the account's whole event state, not this object.
    assert!(args.get("ifInState").is_none());
}

#[tokio::test]
async fn a_create_with_a_location_posts_a_jscalendar_locations_map() {
    // JSCalendar has no scalar location — it is a map of id -> Location (RFC 8984 §4.2.5).
    // A create mints the sole entry, at the same fixed id a later location edit reuses, so
    // the read path's `parse_locations` lands the name back in the projection.
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    )
    .location("Room A");

    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["create"]["new"]["locations"],
        json!({ "1": { "@type": "Location", "name": "Room A" } }),
    );
}

#[tokio::test]
async fn an_all_day_create_states_the_day_not_a_midnight_instant() {
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Company offsite",
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 1).unwrap()),
        // The end is exclusive: a one-day event on the 1st ends on the 2nd.
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 2).unwrap()),
        stamp(),
    );
    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    let object = &args["create"]["new"];
    assert_eq!(object["start"], json!("2026-08-01T00:00:00"));
    assert_eq!(object["timeZone"], Value::Null);
    assert_eq!(
        object["showWithoutTime"],
        json!(true),
        "without this flag the server reads an all-day event as midnight in some zone"
    );
    assert_eq!(object["duration"], json!("P1D"));
}

#[tokio::test]
async fn a_create_the_server_neither_confirmed_nor_rejected_is_a_conflict() {
    let p = provider(vec![set_response(&json!({ "created": {} }))]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        stamp(),
    );
    let err = p.create_event(&account(), &draft).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn destroy_removes_the_event() {
    let (p, exec) = recording(vec![set_response(&json!({ "destroyed": [EVENT] }))]);
    let base = base();
    p.delete_event(&account(), &EventDeletion::of(&base))
        .await
        .unwrap();

    let (_, method, args) = exec.sole_call();
    assert_eq!(method, "CalendarEvent/set");
    assert_eq!(args, json!({ "accountId": "c", "destroy": [EVENT] }));
}

#[tokio::test]
async fn destroying_an_already_gone_event_is_idempotent_success() {
    // The desired end state already holds. Treating `notFound` as a failure here would make
    // a retry of a delete whose response was lost report a hard error — and the outbox's
    // "a recovery retry is safe" promise depends on this, exactly as CalDAV's does on
    // treating a `404`/`410` as success.
    let p = provider(vec![set_response(
        &json!({ "notDestroyed": { EVENT: { "type": "notFound" } } }),
    )]);
    let base = base();
    p.delete_event(&account(), &EventDeletion::of(&base))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_forbidden_destroy_still_surfaces() {
    // Only `notFound` is the idempotent-gone case; a real refusal is not swallowed by it.
    let p = provider(vec![set_response(
        &json!({ "notDestroyed": { EVENT: { "type": "forbidden" } } }),
    )]);
    let base = base();
    let err = p
        .delete_event(&account(), &EventDeletion::of(&base))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn the_calendar_is_writable_but_reports_no_lost_update_guard() {
    // The honesty requirement. JMAP *can* write, so the capability is on — but it cannot
    // refuse a stale edit, so the guard is `Absent`, and a host reads that **before** it
    // writes. A write API that looked like it gave optimistic concurrency everywhere, when
    // here it gives none, is the one outcome this design exists to prevent.
    let p = provider(vec![]);
    let caps = p.connection_info().capabilities;
    assert!(caps.calendars() && caps.calendar_writes());
    assert_eq!(caps.calendar_write_guard(), Some(WriteGuard::Absent));

    // And CalDAV, the transport that *can* promise it, says so differently.
    assert_ne!(
        Capabilities::none()
            .with_calendar_writes(WriteGuard::Enforced)
            .calendar_write_guard(),
        caps.calendar_write_guard()
    );
}

#[tokio::test]
async fn a_read_only_calendar_account_advertises_no_writes() {
    let session = json!({
        "capabilities": {
            "urn:ietf:params:jmap:core": {},
            "urn:ietf:params:jmap:calendars": {}
        },
        "primaryAccounts": { "urn:ietf:params:jmap:calendars": "c" },
        "accounts": { "c": { "isReadOnly": true } },
        "apiUrl": "https://mail.test.local/jmap/"
    });
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(&session, vec![])));
    let caps = p.connection_info().capabilities;
    assert!(caps.calendars());
    assert!(!caps.calendar_writes());
    assert_eq!(caps.calendar_write_guard(), None);
}

#[tokio::test]
async fn there_is_no_whole_document_write_verb_on_this_transport() {
    // A JSCalendar object is not a file whose bytes the client owns, so JMAP leaves
    // `put_event` at the trait's rejecting default *even though* it advertises calendar
    // writes — the capability covers the neutral spine, not the document escape hatch.
    let p = provider(vec![]);
    let base = base();
    let err = p
        .put_event(
            &account(),
            &EventWrite::replacing(&base, engine_core::raw::RawIcal::new("BEGIN:VCALENDAR")),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

/// The base event with two participants, keyed the way a JMAP server keys them: by an
/// **opaque id**, not by the address. Resolving that id is the whole difficulty of a JMAP
/// RSVP.
fn invited() -> Event {
    stored(&json!({
        "@type": "Event",
        "id": EVENT,
        "uid": "evt-1@test.local",
        "title": "Sprint planning",
        "start": "2026-08-01T09:00:00",
        "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
        "participants": {
            "d7a1": {
                "@type": "Participant",
                "calendarAddress": "mailto:boss@test.local",
                "roles": { "owner": true },
                "participationStatus": "accepted",
            },
            "9f0c": {
                "@type": "Participant",
                "calendarAddress": "MAILTO:Info@Example.com",
                "roles": { "attendee": true },
                "participationStatus": "needs-action",
                "expectReply": true,
            },
        },
    }))
}

#[tokio::test]
async fn an_rsvp_patches_only_my_participation_status() {
    // One JSON pointer, so the organizer's participant, my other fields, and every property
    // the engine does not model are left exactly as the server holds them.
    let (p, exec) = recording(vec![set_response(
        &json!({ "updated": { EVENT: Value::Null } }),
    )]);
    let base = invited();

    let receipt = p
        .rsvp_event(
            &account(),
            &base,
            &EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted),
        )
        .await
        .unwrap();

    assert_eq!(receipt.event, EventId::try_from(EVENT).unwrap());
    assert!(receipt.revisions.is_empty());

    let (_using, method, args) = exec.sole_call();
    assert_eq!(method, "CalendarEvent/set");
    assert_eq!(
        args,
        json!({
            "accountId": "c",
            "update": { EVENT: { "participants/9f0c/participationStatus": "accepted" } },
        }),
        "an RSVP must patch my participant's status by its map key and touch nothing else"
    );
}

#[tokio::test]
async fn the_participant_is_matched_by_address_not_by_case_or_scheme() {
    // The stored `calendarAddress` is `MAILTO:Info@Example.com` while the invitation matched
    // `info@example.com`. Comparing the strings would find nobody and the answer would fail
    // on an event the user is plainly invited to.
    let base = invited();
    assert_eq!(
        my_participant_id(&base, "info@example.com").unwrap(),
        "9f0c"
    );
    assert_eq!(
        my_participant_id(&base, "INFO@example.com").unwrap(),
        "9f0c"
    );
    assert_eq!(
        my_participant_id(&base, "mailto:info@example.com").unwrap(),
        "9f0c"
    );
}

#[test]
fn answering_an_invitation_you_are_not_on_is_refused() {
    // Better a refusal than a patch at a pointer the server does not have, which is an
    // `invalidPatch` the user cannot act on — or worse, a silent no-op.
    let err = my_participant_id(&invited(), "stranger@example.com").unwrap_err();
    assert_eq!(
        engine_provider::ProviderError::from(err).class(),
        FailureClass::Permanent
    );
}

#[tokio::test]
async fn jmap_refuses_the_two_controls_its_server_decides() {
    // The server schedules the reply itself, so "don't notify" is not ours to promise; and a
    // `participationComment` we have never seen a server relay is not a note we can claim to
    // have sent. Both refused, not dropped.
    let (p, exec) = recording(vec![set_response(
        &json!({ "updated": { EVENT: Value::Null } }),
    )]);
    let base = invited();

    let controls = p
        .connection_info()
        .capabilities
        .calendar_rsvp()
        .expect("jmap can rsvp");
    assert!(!controls.comment);
    assert!(!controls.suppress_notification);

    for rsvp in [
        EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted).comment("Hi"),
        EventRsvp::to(&base, "info@example.com", RsvpResponse::Declined).quietly(),
    ] {
        let err = p.rsvp_event(&account(), &base, &rsvp).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::InvalidState);
    }
    // Refused before the network, never after a half-applied write.
    assert_eq!(exec.request_count(), 0);
}
