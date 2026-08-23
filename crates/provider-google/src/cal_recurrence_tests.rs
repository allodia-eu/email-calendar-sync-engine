//! Offline tests for the **recurring** halves of a Google calendar write: the `RRULE` line
//! a create and a patch carry, and the derived id a per-occurrence delete addresses.
//!
//! Split from `cal_write_tests` to keep both files under the 500-line cap.

use engine_core::{
    error::FailureClass,
    ids::Uid,
    time::{CalendarDate, CalendarDateTime},
};
use engine_provider::{DraftRecurrence, EventDeletion, Occurrence};
use serde_json::json as sjson;

use super::{cal_write_tests::*, *};
use crate::test_support::fake_client_fallible;

fn weekly_on_monday() -> engine_core::calendar::RecurrenceRule {
    let mut rule =
        engine_core::calendar::RecurrenceRule::new(engine_core::calendar::Frequency::Weekly);
    rule.by_day = vec![engine_core::calendar::NDay {
        day: engine_core::calendar::Weekday::Mo,
        nth_of_period: None,
    }];
    rule
}

#[test]
fn build_create_writes_the_rule_as_an_rrule_line() {
    // Google's `recurrence` is an array of raw iCalendar lines, so what lands here is the
    // same string CalDAV writes — one renderer, two transports.
    let body = build_create(&draft().repeating(DraftRecurrence::new(weekly_on_monday()))).unwrap();
    assert_eq!(body["recurrence"], sjson!(["RRULE:FREQ=WEEKLY;BYDAY=MO"]));
}

#[test]
fn build_create_omits_recurrence_for_a_one_off() {
    assert!(build_create(&draft()).unwrap().get("recurrence").is_none());
}

#[test]
fn build_create_needs_a_resolved_instant_for_a_zoned_until() {
    // RFC 5545 §3.3.10: a zoned start obliges UNTIL in UTC. Refusing is what stops the
    // series ending on a different day for every reader outside Europe/Amsterdam.
    let mut rule = weekly_on_monday();
    rule.bound =
        engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());

    let unresolved = build_create(&draft().repeating(DraftRecurrence::new(rule.clone())));
    assert_eq!(
        unresolved.unwrap_err().class(),
        FailureClass::InvalidState,
        "a zoned UNTIL must not be guessed"
    );

    // 23:59:59 in Europe/Amsterdam is 22:59:59Z.
    let resolved = build_create(&draft().repeating(DraftRecurrence::ending_at(
        rule,
        "2026-10-26T22:59:59Z".parse().unwrap(),
    )))
    .unwrap();
    assert_eq!(
        resolved["recurrence"],
        sjson!(["RRULE:FREQ=WEEKLY;UNTIL=20261026T225959Z;BYDAY=MO"])
    );
}

#[test]
fn build_create_writes_an_all_day_series_until_as_a_date() {
    let mut rule = weekly_on_monday();
    rule.bound =
        engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    let all_day = EventDraft::new(
        calendar(),
        Uid::new("draft-uid@test.local").unwrap(),
        "Offsite",
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 3).unwrap()),
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 4).unwrap()),
        stamp(),
    )
    .repeating(DraftRecurrence::new(rule));
    assert_eq!(
        build_create(&all_day).unwrap()["recurrence"],
        sjson!(["RRULE:FREQ=WEEKLY;UNTIL=20261026;BYDAY=MO"])
    );
}

#[test]
fn build_patch_sets_and_clears_the_recurrence() {
    let set = build_patch(
        &base_event(),
        &EventPatch::new(stamp()).recurrence(DraftRecurrence::new(weekly_on_monday())),
    )
    .unwrap();
    assert_eq!(set["recurrence"], sjson!(["RRULE:FREQ=WEEKLY;BYDAY=MO"]));

    // An empty array clears it. (Google accepts `null` for this too — measured — so this
    // pins the spelling we send, not a difference in what the server does.)
    let cleared = build_patch(&base_event(), &EventPatch::new(stamp()).clear_recurrence()).unwrap();
    assert_eq!(cleared["recurrence"], sjson!([]));
}

#[test]
fn a_patch_that_does_not_mention_recurrence_leaves_it_alone() {
    let body = build_patch(&base_event(), &EventPatch::new(stamp()).summary("Renamed")).unwrap();
    assert!(body.get("recurrence").is_none());
}

#[test]
fn an_occurrence_is_addressed_by_its_original_start_in_utc() {
    // Google's id for an instance is the series id, an underscore, and the original start
    // in a compact form — UTC for a timed series, the bare date for an all-day one.
    let occurrence = Occurrence::at(
        zoned("2026-08-08T09:00:00"),
        "2026-08-08T07:00:00Z".parse().unwrap(),
    );
    assert_eq!(
        occurrence_id("evt-1", &occurrence).unwrap(),
        "evt-1_20260808T070000Z"
    );
    assert_eq!(
        occurrence_id(
            "evt-1",
            &Occurrence::starting(CalendarDateTime::Date(
                engine_core::time::CalendarDate::new(2026, 8, 8).unwrap()
            ))
        )
        .unwrap(),
        "evt-1_20260808"
    );
}

#[test]
fn a_timed_occurrence_with_no_resolved_instant_is_refused() {
    // The wall clock written as if it were UTC would name a different occurrence, or none —
    // and Google answers `404` for "none", which this verb reads as already gone. That would
    // report a delete that never happened.
    let err =
        occurrence_id("evt-1", &Occurrence::starting(zoned("2026-08-08T09:00:00"))).unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
    assert!(err.to_string().contains("Occurrence::at"), "{err}");
}

#[tokio::test]
async fn removing_one_occurrence_deletes_that_occurrence_not_the_series() {
    let base = base_event();
    let deletion = EventDeletion::occurrence(
        &base,
        Occurrence::at(
            zoned("2026-08-08T09:00:00"),
            "2026-08-08T07:00:00Z".parse().unwrap(),
        ),
        stamp(),
    );
    let client = fake_client_fallible(vec![(
        "/events/evt-1_20260808T070000Z",
        Ok(serde_json::Value::Null),
    )]);
    delete_event(&client, "primary", &deletion).await.unwrap();
}
