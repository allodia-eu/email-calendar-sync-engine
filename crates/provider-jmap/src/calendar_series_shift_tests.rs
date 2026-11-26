//! Moving a series moves the keys of its overrides with it (`SeriesShift`).
//!
//! A `recurrenceOverrides` entry is keyed by the occurrence's **original** start (RFC 8984
//! §4.3.3). Move the series and an entry still keyed by the old slot names no occurrence: the
//! expander draws it as an added occurrence beside the one it was made for, which comes back.
//! Observed on Stalwart as one occurrence drawn twice after "all events" was dragged.

use engine_core::calendar::{Frequency, RecurrenceOverride, RecurrenceRule};
use engine_provider::{CalendarWrites, EventPatch, PatchTarget};
use serde_json::json;

use super::{calendar_write_support::*, provider_test_support::*};

/// A weekly series whose overrides cover each kind: renamed only, moved, and excluded.
fn series_with_overrides() -> engine_core::calendar::Event {
    let mut event = stored(&json!({
        "@type": "Event",
        "id": EVENT,
        "uid": "evt-1@test.local",
        "title": "Standup",
        "start": "2026-08-01T09:00:00",
        "timeZone": "Europe/Amsterdam",
        "duration": "PT30M",
        "recurrenceRules": [{ "@type": "RecurrenceRule", "frequency": "weekly" }],
        "recurrenceOverrides": {
            "2026-08-08T09:00:00": { "title": "Renamed once" },
            "2026-08-15T09:00:00": { "start": "2026-08-15T14:00:00" },
            "2026-08-22T09:00:00": { "excluded": true },
        },
    }));
    let mut recurrence =
        engine_core::calendar::Recurrence::from_rule(RecurrenceRule::new(Frequency::Weekly));
    for key in ["2026-08-08T09:00:00", "2026-08-15T09:00:00"] {
        recurrence.overrides.insert(
            key.parse().unwrap(),
            RecurrenceOverride::Patch(
                engine_core::patch::PatchObject::new([("title".to_owned(), json!("x"))]).unwrap(),
            ),
        );
    }
    recurrence.overrides.insert(
        "2026-08-22T09:00:00".parse().unwrap(),
        RecurrenceOverride::Excluded,
    );
    event.recurrence = Some(recurrence);
    event
}

#[tokio::test]
async fn moving_the_series_moves_every_override_key_with_it() {
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = series_with_overrides();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp())
                .start(zoned("2026-08-01T10:30:00"))
                .end(zoned("2026-08-01T11:00:00")),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    let update = &args["update"][EVENT];
    assert_eq!(update["start"], json!("2026-08-01T10:30:00"));
    assert_eq!(
        update["recurrenceOverrides"],
        json!({
            "2026-08-08T10:30:00": { "title": "Renamed once" },
            "2026-08-15T10:30:00": { "start": "2026-08-15T14:00:00" },
            "2026-08-22T10:30:00": { "excluded": true },
        }),
        "each entry moves to its occurrence's new slot, its own contents untouched: one the \
         user moved keeps the time they gave it"
    );
}

#[tokio::test]
async fn a_series_edit_that_moves_no_slot_leaves_the_overrides_alone() {
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let base = series_with_overrides();
    p.patch_event(
        &account(),
        &base,
        &edit(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp())
                .start(zoned("2026-08-01T09:00:00"))
                .end(zoned("2026-08-01T10:00:00")),
        ),
    )
    .await
    .unwrap();

    let (_, _, args) = exec.sole_call();
    assert!(
        args["update"][EVENT].get("recurrenceOverrides").is_none(),
        "a resize moves no occurrence, so no key moves"
    );
}
