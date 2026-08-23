//! Gated live checks for the writes that target **one occurrence** of a Google series —
//! editing one, removing one, and reading either back off the series.
//!
//! Split from `live_calendar_recurrence.rs`, which targets the series itself, to keep both
//! under the 500-line cap; the shared setup lives in `common`.

mod common;

use common::*;
use engine_core::{
    calendar::Event,
    ids::{CalendarId, Uid},
    membership::Memberships,
    sync::SyncUpdate,
    time::UtcDateTime,
};
use engine_provider::{EventDeletion, EventDraft, Provider};

/// Removing **one occurrence** of a series, at the id Google derives from its original
/// start in UTC.
///
/// The read-back is the whole test, and not a formality: a `DELETE` of an id that names no
/// occurrence answers `404`, and this verb reads `404` as "already gone" — correctly, for an
/// idempotent delete. So an id built from the wall clock as if it were UTC would report a
/// delete that never happened, and only the server's own account of what changed can tell
/// the two apart. Here that account is the **series' own override map**: Google returns the
/// cancelled instance as a `status: "cancelled"` entry, which the reader folds onto the
/// series it names.
#[tokio::test]
async fn live_calendar_removes_one_occurrence() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, RecurrenceBound, RecurrenceRule};
    use engine_provider::{DraftRecurrence, Occurrence};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_removes_one_occurrence: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();

    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                cal.clone(),
                Uid::new(format!("live-occ-{}@example.test", std::process::id())).unwrap(),
                "Live occurrence-delete probe",
                zoned("2026-09-07T09:30:00"),
                zoned("2026-09-07T10:00:00"),
                stamp,
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    // The cursor is taken *after* the create, so the delta below carries the cancellation.
    let cursor = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events")
        .next_cursor;

    let mut base = Event::new(
        created.event.clone(),
        created.uid.clone(),
        Memberships::of_one(cal.clone()),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();

    // 09:30 Amsterdam on 14 September is 07:30Z — CEST, and the resolution the caller owns.
    provider
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(
                &base,
                Occurrence::at(
                    zoned("2026-09-14T09:30:00"),
                    "2026-09-14T07:30:00Z".parse().unwrap(),
                ),
                stamp,
            ),
        )
        .await
        .expect("remove one occurrence");

    let delta = provider
        .sync_events(&account(), Some(&cursor))
        .await
        .expect("sync the change");
    let SyncUpdate::Delta {
        changed, removed, ..
    } = &delta.update
    else {
        panic!("expected a delta");
    };
    let series = changed
        .iter()
        .find(|e| e.id == created.event)
        .expect("the change reached the series, not an object of the occurrence's own");
    assert_eq!(
        series
            .recurrence
            .as_ref()
            .expect("a series")
            .overrides
            .get(&"2026-09-14T09:30:00".parse().unwrap()),
        Some(&engine_core::calendar::RecurrenceOverride::Excluded),
        "the occurrence is excluded from the series"
    );
    assert!(
        !removed.iter().any(|key| key == created.event.key()),
        "and the series itself was not deleted: {removed:?}"
    );

    // Unguarded: cancelling an occurrence moved the series' own ETag, and this cleanup is
    // not the thing under test.
    provider
        .delete_event(
            &account(),
            None,
            &EventDeletion::unconditional(created.event.clone(), created.uid.clone()),
        )
        .await
        .expect("delete the probe series");
}

/// A series with an occurrence removed, read back with its **exclusion on the series** and
/// the rest of the rule intact.
///
/// This is the half that makes the write half worth having: until the entries Google returns
/// for changed occurrences are folded into the master's override map, an occurrence the user
/// deleted keeps being drawn.
///
/// Both halves are here: an occurrence moved and renamed through `PatchTarget::Instance`, and
/// one removed. The move is the sharper of the two — it is read back as a **patch keyed by
/// the start it used to have**, so a reader that keyed by the new start would report an
/// override of an instant the rule never produces, and draw the occurrence twice.
#[tokio::test]
async fn live_calendar_reads_a_moved_and_a_removed_occurrence_off_the_series() {
    use core::num::NonZeroU32;

    use engine_core::calendar::{Frequency, RecurrenceBound, RecurrenceOverride, RecurrenceRule};
    use engine_provider::{DraftRecurrence, EventEdit, EventPatch, Occurrence, PatchTarget};

    let Some(token) = token() else {
        eprintln!("skipping live_calendar_reads_…: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = calendar_provider(token);
    let cal = CalendarId::try_from("primary").unwrap();
    let stamp: UtcDateTime = "2026-08-23T10:00:00Z".parse().unwrap();

    let mut weekly = RecurrenceRule::new(Frequency::Weekly);
    weekly.bound = RecurrenceBound::Count(NonZeroU32::new(6).unwrap());

    let created = provider
        .create_event(
            &account(),
            &EventDraft::new(
                cal.clone(),
                Uid::new(format!("live-ovr-{}@example.test", std::process::id())).unwrap(),
                "Live override-read probe",
                zoned("2026-09-07T09:30:00"),
                zoned("2026-09-07T10:00:00"),
                stamp,
            )
            .repeating(DraftRecurrence::new(weekly)),
        )
        .await
        .expect("create a recurring event");

    let mut base = Event::new(
        created.event.clone(),
        created.uid.clone(),
        Memberships::of_one(cal.clone()),
        zoned("2026-09-07T09:30:00"),
    );
    base.revisions = created.revisions.clone();

    // Move the 14th to the afternoon and rename it…
    provider
        .patch_event(
            &account(),
            &base,
            &EventEdit::new(
                &base,
                PatchTarget::Instance(Occurrence::at(
                    zoned("2026-09-14T09:30:00"),
                    "2026-09-14T07:30:00Z".parse().unwrap(),
                )),
                EventPatch::new(stamp)
                    .summary("Moved to the afternoon")
                    .start(zoned("2026-09-14T14:00:00"))
                    .end(zoned("2026-09-14T14:45:00")),
            ),
        )
        .await
        .expect("edit one occurrence");

    // …and remove the 21st.
    provider
        .delete_event(
            &account(),
            Some(&base),
            &EventDeletion::occurrence(
                &base,
                Occurrence::at(
                    zoned("2026-09-21T09:30:00"),
                    "2026-09-21T07:30:00Z".parse().unwrap(),
                ),
                stamp,
            ),
        )
        .await
        .expect("remove one occurrence");

    let events = provider
        .sync_events(&account(), None)
        .await
        .expect("sync events");
    let SyncUpdate::Snapshot { objects, .. } = &events.update else {
        panic!("expected an event snapshot");
    };
    let series = objects
        .iter()
        .find(|e| e.id == created.event)
        .expect("the series is in the snapshot");
    let recurrence = series.recurrence.as_ref().expect("a series");
    let RecurrenceOverride::Patch(patch) = recurrence
        .overrides
        .get(&"2026-09-14T09:30:00".parse().unwrap())
        .expect("the moved occurrence, keyed by the start it used to have")
    else {
        panic!("a moved occurrence is a patch, not an exclusion");
    };
    assert_eq!(patch.get("start").unwrap(), "2026-09-14T14:00:00");
    assert_eq!(patch.get("duration").unwrap(), "PT45M");
    assert_eq!(patch.get("title").unwrap(), "Moved to the afternoon");
    assert_eq!(
        recurrence
            .overrides
            .get(&"2026-09-21T09:30:00".parse().unwrap()),
        Some(&RecurrenceOverride::Excluded),
        "the deleted occurrence is excluded rather than still drawn"
    );
    assert_eq!(
        recurrence.overrides.len(),
        2,
        "and nothing else was mistaken for an override: {:?}",
        recurrence.overrides
    );
    assert_eq!(
        series.title, "Live override-read probe",
        "the series itself was never touched — an id that resolved to it would have \
         renamed every occurrence"
    );

    provider
        .delete_event(
            &account(),
            None,
            &EventDeletion::unconditional(created.event.clone(), created.uid.clone()),
        )
        .await
        .expect("delete the probe series");
}
