//! Offline tests for **creating a recurring event** over JMAP: the `recurrenceRule` object
//! the create posts, and the local-`until` reading that sets JSCalendar apart from
//! iCalendar.
//!
//! Split from `calendar_write_tests` to keep both files under the 500-line cap.

use engine_provider::{DraftRecurrence, EventDraft, EventPatch, PatchTarget};
use serde_json::json;

use super::{calendar_write_support::*, provider_test_support::*, *};

// ---------------------------------------------------------------------------

fn weekly_on_monday() -> engine_core::calendar::RecurrenceRule {
    let mut rule =
        engine_core::calendar::RecurrenceRule::new(engine_core::calendar::Frequency::Weekly);
    rule.by_day = vec![engine_core::calendar::NDay {
        day: engine_core::calendar::Weekday::Mo,
        nth_of_period: None,
    }];
    rule
}

#[tokio::test]
async fn create_posts_the_rule_as_a_jscalendar_recurrence_rules_array() {
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Standup",
        zoned("2026-08-03T09:00:00"),
        zoned("2026-08-03T09:30:00"),
        stamp(),
    )
    .repeating(DraftRecurrence::new(weekly_on_monday()));

    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["create"]["new"]["recurrenceRule"],
        json!({
            "@type": "RecurrenceRule",
            "frequency": "weekly",
            "byDay": [{ "@type": "NDay", "day": "mo" }],
        })
    );
}

#[tokio::test]
async fn create_omits_recurrence_rules_for_a_one_off() {
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Once",
        zoned("2026-08-03T09:00:00"),
        zoned("2026-08-03T09:30:00"),
        stamp(),
    );

    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    assert!(args["create"]["new"].get("recurrenceRule").is_none());
}

#[tokio::test]
async fn a_zoned_until_stays_a_local_wall_clock_on_jmap() {
    // RFC 8984 §4.3.3 reads `until` in the event's own zone — the opposite of iCalendar's
    // UTC rule. So a draft CalDAV would refuse for want of a resolved instant is written
    // here verbatim, and converting it would move the end of the series.
    let (p, exec) = recording(vec![set_response(
        &json!({ "created": { "new": { "id": EVENT } } }),
    )]);
    let mut rule = weekly_on_monday();
    rule.bound =
        engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Standup",
        zoned("2026-08-03T09:00:00"),
        zoned("2026-08-03T09:30:00"),
        stamp(),
    )
    .repeating(DraftRecurrence::new(rule));

    p.create_event(&account(), &draft).await.unwrap();

    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["create"]["new"]["recurrenceRule"]["until"],
        "2026-10-26T23:59:59"
    );
}

// ---------------------------------------------------------------------------
// Editing the rule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recurrence_edit_sets_or_removes_the_singular_property() {
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    p.patch_event(
        &account(),
        &base(),
        &edit(
            &base(),
            PatchTarget::Series,
            EventPatch::new(stamp()).recurrence(DraftRecurrence::new(weekly_on_monday())),
        ),
    )
    .await
    .unwrap();
    let (_, _, args) = exec.sole_call();
    assert_eq!(
        args["update"][EVENT]["recurrenceRule"]["frequency"],
        "weekly"
    );

    // `null` removes a property in an RFC 8620 §5.3 PatchObject — how a series becomes a
    // single event.
    let (p, exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    p.patch_event(
        &account(),
        &base(),
        &edit(
            &base(),
            PatchTarget::Series,
            EventPatch::new(stamp()).clear_recurrence(),
        ),
    )
    .await
    .unwrap();
    let (_, _, args) = exec.sole_call();
    assert!(args["update"][EVENT]["recurrenceRule"].is_null());
}

#[tokio::test]
async fn a_recurrence_edit_cannot_ride_an_instance_target() {
    // Everything else in an Instance patch is prefixed `recurrenceOverrides/<start>/`;
    // a rule written inside one occurrence's override would mean nothing at all.
    let (p, _exec) = recording(vec![set_response(&json!({ "updated": { EVENT: null } }))]);
    let err = p
        .patch_event(
            &account(),
            &base(),
            &edit(
                &base(),
                PatchTarget::Instance(zoned("2026-08-01T09:00:00")),
                EventPatch::new(stamp()).recurrence(DraftRecurrence::new(weekly_on_monday())),
            ),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("targets the series"), "{err}");
}
