//! Offline tests for the **PatchObject** a `CalendarEvent/set` `update` produces — where all
//! the protocol detail of this transport lives. Create/destroy are `calendar_write_tests`.
//!
//! The fake executor replies with canned bytes **whatever it is sent**, so a bad JSON pointer
//! would sail through a test that only checked the receipt (`AGENTS.md`). Each test therefore
//! asserts the produced JSON *literally*. That covers the request's **shape**; whether a real
//! server merges it as we expect is `tests/live_calendar_write.rs`.

use engine_core::{error::FailureClass, ids::EventId, time::CalendarDateTime};
use engine_provider::{EventPatch, PatchTarget};
use serde_json::json;

use super::{calendar_write_support::*, provider_test_support::*, *};

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
        json!({
            "accountId": "c",
            "update": { EVENT: { "title": "Renamed" } },
            "sendSchedulingMessages": true,
        }),
        "rescheduling or renaming a meeting must reach its participants; the server sends \
         nothing unless asked"
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
async fn editing_an_already_overridden_occurrence_patches_through_its_pointer() {
    // JSCalendar names an occurrence by its *original* start under `recurrenceOverrides`
    // (RFC 8984 §4.3.3), and the **server** materializes the override from the series. That
    // is CalDAV's whole RECURRENCE-ID-splitting chore, done server-side — which is exactly
    // why the neutral `PatchTarget::Instance` must not promise CalDAV's start+end.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = overridden_base("2026-08-08T09:00:00");
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
async fn a_first_edit_of_an_occurrence_assigns_the_override_map() {
    // A series nobody has touched has no `recurrenceOverrides` at all, and RFC 8620 §5.3
    // lets a pointer address only what already exists — so the pointer form above would be
    // rejected *whole*, taking the edit with it. The first edit therefore assigns the map.
    // The occurrence's own properties keep their names inside it: the entry is itself a
    // PatchObject.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = recurring_base();
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
            "recurrenceOverrides": {
                "2026-08-08T09:00:00": {
                    "title": "Skipped standup",
                    "start": "2026-08-08T10:00:00",
                },
            },
        }),
        "the map is keyed by the occurrence's original start, not the new one"
    );
}

#[tokio::test]
async fn a_first_edit_that_adds_a_location_keeps_its_pointer_inside_the_override() {
    // The one shape that could have been mangled by nesting: a location edit is already a
    // pointer, and an override entry is a PatchObject, so it goes in verbatim rather than
    // being expanded into nested objects.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = recurring_base();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Instance(zoned("2026-08-08T09:00:00")),
            EventPatch::new(stamp()).location("Room B"),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT],
        json!({
            "recurrenceOverrides": {
                "2026-08-08T09:00:00": {
                    "locations/1": { "@type": "Location", "name": "Room B" },
                },
            },
        }),
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
