//! Gated live checks that the overrides of a series stay what the user made them, against Stalwart.
//!
//! A PatchObject pointer may address only what already exists (RFC 8620 §5.3), and the shape of
//! an occurrence's edit depends on what the series already holds: no map, a map without this
//! occurrence, or a map with it. The fake executor accepts every shape, so only a server can say
//! which one it merges. Skips with no `STALWART_HTTP_ADDR`.

mod common;

use core::num::NonZeroU32;

use common::*;
use engine_core::{
    calendar::{Frequency, RecurrenceBound, RecurrenceOverride, RecurrenceRule},
    ids::Uid,
    time::LocalDateTime,
};
use engine_provider::{
    CalendarWrites, DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, Occurrence,
    PatchTarget,
};

const SERIES_START: &str = "2026-07-06T09:30:00";
const FIRST_MOVED: &str = "2026-07-13T09:30:00";
const SECOND_MOVED: &str = "2026-07-20T09:30:00";

#[tokio::test]
async fn a_second_occurrence_is_overridden_beside_the_first() {
    let Some(provider) = setup("a_second_occurrence_is_overridden_beside_the_first").await else {
        return;
    };
    let uid = "live-jmap-second-override@test.local";
    pre_clean(&provider, uid).await;
    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());
    provider
        .create_event(
            &account(),
            &EventDraft::new(
                calendar(&provider).await,
                Uid::new(uid).unwrap(),
                "Live JMAP override probe",
                amsterdam(SERIES_START),
                amsterdam("2026-07-06T10:00:00"),
                stamp(),
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    // The first occurrence the user moves assigns the map; the second adds its own entry to it.
    for (occurrence, start, end) in [
        (FIRST_MOVED, "2026-07-13T11:00:00", "2026-07-13T11:30:00"),
        (SECOND_MOVED, "2026-07-20T14:00:00", "2026-07-20T14:30:00"),
    ] {
        let base = require(&provider, uid).await;
        provider
            .patch_event(
                &account(),
                &base,
                &EventEdit::new(
                    &base,
                    PatchTarget::Instance(Occurrence::starting(amsterdam(occurrence))),
                    EventPatch::new(stamp())
                        .start(amsterdam(start))
                        .end(amsterdam(end)),
                ),
            )
            .await
            .unwrap_or_else(|err| panic!("the server accepts the move of {occurrence}: {err}"));
    }

    let stored = require(&provider, uid).await;
    let overrides = stored
        .recurrence
        .as_ref()
        .expect("still a series")
        .overrides
        .clone();
    let moved_to =
        |occurrence: &str| match overrides.get(&occurrence.parse::<LocalDateTime>().unwrap()) {
            Some(RecurrenceOverride::Patch(patch)) => patch
                .get("start")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
    assert_eq!(
        moved_to(FIRST_MOVED).as_deref(),
        Some("2026-07-13T11:00:00"),
        "the first move survives the second"
    );
    assert_eq!(
        moved_to(SECOND_MOVED).as_deref(),
        Some("2026-07-20T14:00:00"),
        "the second move landed in an entry of its own"
    );

    provider
        .delete_event(&account(), None, &EventDeletion::of(&stored))
        .await
        .expect("delete the probe series");
}
