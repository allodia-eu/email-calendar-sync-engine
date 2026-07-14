//! Offline `CalendarEvent/set` tests: create, patch (series and one occurrence), destroy.
//!
//! The fake executor serves its canned reply **whatever it is sent**, so a test that only
//! checked the returned receipt would pass with a completely malformed request
//! (`AGENTS.md`). Every test here therefore asserts the **request** the adapter produced —
//! the method name, the `using` set, and the exact JSON of the arguments — and the canned
//! responses are the shapes Stalwart actually returned when the write path was probed
//! against the live harness. What no offline test can prove is that the server *accepts*
//! it; that is `tests/live_provider.rs`.

use engine_core::{
    calendar::Event,
    error::FailureClass,
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    raw::RawJsCalendar,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
};
use engine_provider::{
    Capabilities, EventDeletion, EventDraft, EventEdit, EventPatch, EventWrite, PatchTarget,
    WriteGuard,
};
use serde_json::{Value, json};

use super::{provider_test_support::*, *};

const CALENDAR: &str = "b";
const EVENT: &str = "l";

fn calendar() -> CalendarId {
    CalendarId::try_from(CALENDAR).unwrap()
}

fn uid() -> Uid {
    Uid::new("evt-1@test.local").unwrap()
}

fn stamp() -> UtcDateTime {
    "2026-07-14T10:00:00Z".parse().unwrap()
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// An event as `sync_events` hands it back: zoned, with its JSCalendar raw preserved.
fn stored(raw: &Value) -> Event {
    let mut event = Event::new(
        EventId::try_from(EVENT).unwrap(),
        uid(),
        Memberships::of_one(calendar()),
        zoned("2026-08-01T09:00:00"),
    );
    event.raw_jscalendar = Some(RawJsCalendar::new(raw.to_string()));
    event
}

/// The base event with no location on it.
fn base() -> Event {
    stored(&json!({
        "@type": "Event",
        "id": EVENT,
        "uid": "evt-1@test.local",
        "title": "Standup",
        "start": "2026-08-01T09:00:00",
        "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
    }))
}

fn set_response(result: &Value) -> Value {
    json!({ "methodResponses": [["CalendarEvent/set", result, "0"]] })
}

fn edit(base: &Event, target: PatchTarget, patch: EventPatch) -> EventEdit {
    EventEdit::new(base, target, patch)
}

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
async fn a_partial_update_sends_only_what_changed() {
    // The finding that makes this whole transport different from CalDAV: `update` is a
    // PatchObject the *server* merges, so renaming an event touches `title` and nothing
    // else. There is no document to rebuild, and so nothing to lose by rebuilding it.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    let receipt = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            ),
        )
        .await
        .unwrap();
    assert_eq!(receipt.event, EventId::try_from(EVENT).unwrap());

    let (_, method, args) = exec.sole_call();
    assert_eq!(method, "CalendarEvent/set");
    assert_eq!(
        args,
        json!({ "accountId": "c", "update": { EVENT: { "title": "Renamed" } } })
    );
}

#[tokio::test]
async fn a_null_updated_value_is_an_acknowledgement_not_a_failure() {
    // `updated: {id: null}` means "applied, with no extra server-set changes" (RFC 8620
    // §5.3). Reading the null as a failure would report a landed write as broken.
    let p = provider(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    assert!(
        p.patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed")
            ),
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn a_move_patches_the_wall_clock_and_the_duration_never_the_zone() {
    // Moving a zoned event rewrites `start` (the wall clock) and re-derives `duration` —
    // JSCalendar has no end. `timeZone` is never touched: this is a move, not a conversion.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp())
                .start(zoned("2026-08-01T14:00:00"))
                .end(zoned("2026-08-01T15:00:00")),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    let patch = &args["update"][EVENT];
    assert_eq!(patch["start"], json!("2026-08-01T14:00:00"));
    assert_eq!(patch["duration"], json!("PT1H"));
    assert!(
        patch.get("timeZone").is_none(),
        "a move must not rewrite the zone: {patch}"
    );
}

#[tokio::test]
async fn a_resize_alone_derives_its_duration_from_the_stored_start() {
    // Only the end moved, so the new duration is measured from the start the event already
    // has — the base is what makes that possible.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).end(zoned("2026-08-01T10:30:00")),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(args["update"][EVENT]["duration"], json!("PT1H30M"));
    assert!(args["update"][EVENT].get("start").is_none());
}

#[tokio::test]
async fn a_move_that_would_change_the_events_time_form_is_refused_not_converted() {
    // The universal rule (`CalendarDateTime::has_same_form`): re-expressing an
    // Amsterdam event as the UTC instant it happens to denote today shifts it for every
    // reader elsewhere and re-times the series at the next DST boundary. Refuse it — and
    // refuse it *before* the network, so nothing lands.
    let (p, exec) = recording(vec![]);
    let base = base();
    let err = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).start(CalendarDateTime::utc(
                    "2026-08-01T12:00:00".parse().unwrap(),
                )),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(
        exec.requests.lock().unwrap().is_empty(),
        "a rejected patch must never reach the network"
    );
}

#[tokio::test]
async fn an_end_before_the_start_is_refused() {
    let (p, exec) = recording(vec![]);
    let base = base();
    let err = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).end(zoned("2026-08-01T08:00:00")),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(exec.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn editing_one_occurrence_patches_under_the_recurrence_override() {
    // JSCalendar names an occurrence by its *original* start under `recurrenceOverrides`
    // (RFC 8984 §4.3.3), and the **server** materializes the override from the series. That
    // is CalDAV's whole RECURRENCE-ID-splitting chore, done server-side — which is exactly
    // why the neutral `PatchTarget::Instance` must not promise CalDAV's start+end.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Instance(zoned("2026-08-08T09:00:00")),
            EventPatch::new(stamp())
                .summary("Skipped standup")
                .start(zoned("2026-08-08T10:00:00")),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT],
        json!({
            "recurrenceOverrides/2026-08-08T09:00:00/title": "Skipped standup",
            "recurrenceOverrides/2026-08-08T09:00:00/start": "2026-08-08T10:00:00",
        }),
        "every pointer must sit under the occurrence's original start, not the new one"
    );
}

#[tokio::test]
async fn an_occurrence_named_in_another_time_form_overrides_nothing_and_is_refused() {
    // The recurrence id is the occurrence's identity within the series, so it must be
    // expressed the way the series is. A UTC instant naming "the same moment" as the zoned
    // occurrence keys an override the series has no instance at — a silent no-op.
    let (p, exec) = recording(vec![]);
    let base = base();
    let err = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Instance(CalendarDateTime::utc(
                    "2026-08-08T07:00:00".parse().unwrap(),
                )),
                EventPatch::new(stamp()).summary("Renamed"),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(exec.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn renaming_a_location_patches_the_one_already_on_the_event() {
    // JSCalendar has no scalar location: `locations` is a map of id → Location. Renaming
    // "the location" therefore means patching `locations/<its id>/name` — which keeps that
    // location's coordinates and every other location the event has. Replacing the whole
    // map (the lazy reading of a scalar edit) would silently discard them. The id lives
    // only in the preserved raw, which is why the read path keeps it.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = stored(&json!({
        "@type": "Event",
        "id": EVENT,
        "uid": "evt-1@test.local",
        "start": "2026-08-01T09:00:00",
        "timeZone": "Europe/Amsterdam",
        "locations": {
            "loc-a": { "@type": "Location", "name": "Room A", "coordinates": "geo:52.37,4.89" }
        },
    }));
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).location("Room B"),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT],
        json!({ "locations/loc-a/name": "Room B" }),
        "the edit must target the existing location's name, so its coordinates survive"
    );
}

#[tokio::test]
async fn giving_a_location_to_an_event_that_has_none_adds_the_whole_object() {
    // A pointer *into* a map entry the server does not have is an `invalidPatch`, so a
    // first location goes in as a whole Location object at a fresh id.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).location("Room B"),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT],
        json!({ "locations/1": { "@type": "Location", "name": "Room B" } })
    );
}

#[tokio::test]
async fn clearing_a_text_property_sends_null_which_removes_it() {
    // In a PatchObject, `null` *removes* the property (RFC 8620 §5.3) — which is how the
    // neutral three-state TextEdit (set / clear / untouched) lands here.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp())
                .clear_description()
                .clear_location(),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT],
        json!({ "description": null, "locations": null })
    );
}

#[tokio::test]
async fn a_patch_that_changes_nothing_makes_no_request() {
    // An empty edit would otherwise send a no-op `update` the server still bumps state for.
    let (p, exec) = recording(vec![]);
    let base = base();
    let receipt = p
        .patch_event(
            &account(),
            &base,
            &edit(&base, PatchTarget::Series, EventPatch::new(stamp())),
        )
        .await
        .unwrap();
    assert_eq!(receipt.event, EventId::try_from(EVENT).unwrap());
    assert!(exec.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_set_error_on_an_update_classifies() {
    let p = provider(vec![set_response(
        &json!({ "notUpdated": { EVENT: { "type": "forbidden" } } }),
    )]);
    let base = base();
    let err = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_silently_dropped_target_is_a_conflict_never_a_false_success() {
    // The server acknowledged neither our id nor a failure for it. Reporting success would
    // tell the user their edit saved when it did not.
    let p = provider(vec![set_response(
        &json!({ "updated": { "other": null }, "notUpdated": {} }),
    )]);
    let base = base();
    let err = p
        .patch_event(
            &account(),
            &base,
            &edit(
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
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
