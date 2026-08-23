//! Offline tests for calendar writes: the create/patch bodies, the form guard, and the
//! create/patch/delete flows over the fake transport.

use engine_core::{
    calendar::Event,
    error::FailureClass,
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, PatchTarget,
    RsvpResponse,
};
use serde_json::json as sjson;

use super::*;
use crate::test_support::fake_client_fallible;

fn calendar() -> CalendarId {
    CalendarId::try_from("cal-1").unwrap()
}

fn stamp() -> UtcDateTime {
    "2026-07-18T10:00:00Z".parse().unwrap()
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

fn draft() -> EventDraft {
    EventDraft::new(
        calendar(),
        Uid::new("draft-uid@test.local").unwrap(),
        "Sprint planning",
        zoned("2026-08-03T09:00:00"),
        zoned("2026-08-03T09:30:00"),
        stamp(),
    )
    .location("Room A")
    .description("agenda")
}

fn base_event() -> Event {
    let mut event = Event::new(
        EventId::try_from("evt-1").unwrap(),
        Uid::new("evt-1@test.local").unwrap(),
        Memberships::of_one(calendar()),
        zoned("2026-08-03T09:00:00"),
    );
    event.revisions = RevisionTokens::from_etag(ETag::new("W/\"v7\""));
    event
}

/// A Graph created/updated event response.
fn stored(id: &str, ical_uid: &str, etag: &str) -> serde_json::Value {
    sjson!({ "id": id, "iCalUId": ical_uid, "@odata.etag": etag, "type": "singleInstance" })
}

#[test]
fn build_create_maps_subject_zone_location_and_description() {
    let body = build_create(&draft()).unwrap();
    assert_eq!(body["subject"], "Sprint planning");
    assert_eq!(body["start"]["dateTime"], "2026-08-03T09:00:00");
    // The IANA zone is sent verbatim (Graph accepts IANA names on write).
    assert_eq!(body["start"]["timeZone"], "Europe/Amsterdam");
    assert_eq!(body["end"]["dateTime"], "2026-08-03T09:30:00");
    assert_eq!(body["location"]["displayName"], "Room A");
    assert_eq!(body["body"]["content"], "agenda");
    assert!(body.get("isAllDay").is_none());
}

#[test]
fn build_create_marks_an_all_day_event() {
    let draft = EventDraft::new(
        calendar(),
        Uid::new("u@test.local").unwrap(),
        "Offsite",
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 10).unwrap()),
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 11).unwrap()),
        stamp(),
    );
    let body = build_create(&draft).unwrap();
    assert_eq!(body["isAllDay"], true);
    assert_eq!(body["start"]["dateTime"], "2026-08-10T00:00:00");
    assert_eq!(body["start"]["timeZone"], "UTC");
}

#[tokio::test]
async fn create_event_returns_the_server_id_uid_and_etag() {
    let client = fake_client_fallible(vec![(
        "/events",
        Ok(stored("srv-id", "SERVER-UID", "W/\"v1\"")),
    )]);
    let receipt = create_event(&client, "/calendars/cal-1", &draft())
        .await
        .unwrap();
    assert_eq!(receipt.event.key().as_str(), "srv-id");
    // Graph assigns the iCalUId (a client UID is not accepted), so the receipt carries
    // the server's, not the draft's.
    assert_eq!(receipt.uid.as_str(), "SERVER-UID");
    assert_eq!(receipt.revisions.etag, Some(ETag::new("W/\"v1\"")));
}

#[test]
fn build_patch_sends_only_changed_fields_and_keeps_the_zone() {
    let patch = EventPatch::new(stamp())
        .summary("Renamed")
        .start(zoned("2026-08-03T10:00:00"));
    let body = build_patch(&base_event(), &patch).unwrap();
    assert_eq!(body["subject"], "Renamed");
    assert_eq!(body["start"]["dateTime"], "2026-08-03T10:00:00");
    assert_eq!(body["start"]["timeZone"], "Europe/Amsterdam");
    // Nothing else is touched.
    assert!(body.get("body").is_none() && body.get("location").is_none());
}

#[test]
fn build_patch_rejects_a_form_change() {
    // Moving a zoned event to a UTC instant is silent corruption — refused.
    let patch = EventPatch::new(stamp()).start(CalendarDateTime::utc(
        "2026-08-03T10:00:00".parse().unwrap(),
    ));
    let err = build_patch(&base_event(), &patch).unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn patch_event_empty_is_a_no_op_without_a_request() {
    // An empty patch neither errors nor calls the server; it reports the base revision.
    let client = fake_client_fallible(vec![]);
    let edit = EventEdit::new(&base_event(), PatchTarget::Series, EventPatch::new(stamp()));
    let receipt = patch_event(&client, &base_event(), &edit).await.unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("W/\"v7\"")));
}

#[tokio::test]
async fn patch_event_rejects_a_per_occurrence_target() {
    let client = fake_client_fallible(vec![]);
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Instance(zoned("2026-08-10T09:00:00")),
        EventPatch::new(stamp()).summary("x"),
    );
    let err = patch_event(&client, &base_event(), &edit)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn patch_event_returns_the_new_etag() {
    let client = fake_client_fallible(vec![(
        "/events/evt-1",
        Ok(stored("evt-1", "evt-1@test.local", "W/\"v8\"")),
    )]);
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let receipt = patch_event(&client, &base_event(), &edit).await.unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("W/\"v8\"")));
}

#[test]
fn build_patch_clears_text_and_moves_the_end() {
    let patch = EventPatch::new(stamp())
        .clear_description()
        .clear_location()
        .end(zoned("2026-08-03T11:00:00"));
    let body = build_patch(&base_event(), &patch).unwrap();
    // A cleared text property writes an empty value (distinct from leaving it alone).
    assert_eq!(body["body"]["content"], "");
    assert_eq!(body["location"]["displayName"], "");
    assert_eq!(body["end"]["dateTime"], "2026-08-03T11:00:00");
    assert!(body.get("subject").is_none());
}

#[test]
fn build_create_rejects_a_floating_start() {
    // Graph has no floating-time events, so a floating draft is refused, not converted.
    let draft = EventDraft::new(
        calendar(),
        Uid::new("u@test.local").unwrap(),
        "floating",
        CalendarDateTime::Floating("2026-08-03T09:00:00".parse().unwrap()),
        CalendarDateTime::Floating("2026-08-03T10:00:00".parse().unwrap()),
        stamp(),
    );
    let err = build_create(&draft).unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn patch_event_falls_back_to_the_base_identity_when_no_body_is_echoed() {
    // A `204`-style patch (no echoed object) still resolves to the base's id/uid.
    let client = fake_client_fallible(vec![("/events/evt-1", Ok(serde_json::Value::Null))]);
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let receipt = patch_event(&client, &base_event(), &edit).await.unwrap();
    assert_eq!(receipt.event.as_str(), "evt-1");
    assert_eq!(receipt.uid.as_str(), "evt-1@test.local");
    assert!(receipt.revisions.etag.is_none());
}

#[tokio::test]
async fn delete_event_is_idempotent_on_404_but_a_conflict_on_412() {
    let deletion = EventDeletion::of(&base_event());
    // Already gone → success.
    let gone = fake_client_fallible(vec![("/events/evt-1", Err((404, sjson!({}))))]);
    assert!(delete_event(&gone, &deletion).await.is_ok());
    // A stale If-Match → a conflict (refetch, then retry).
    let stale = fake_client_fallible(vec![("/events/evt-1", Err((412, sjson!({}))))]);
    let err = delete_event(&stale, &deletion).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
    // A clean delete succeeds.
    let ok = fake_client_fallible(vec![("/events/evt-1", Ok(serde_json::Value::Null))]);
    assert!(delete_event(&ok, &deletion).await.is_ok());
}

#[test]
fn build_rsvp_always_states_whether_the_organizer_is_emailed() {
    // Graph's `sendResponse` defaults to true. Omitting it when the user asked for silence
    // would email the organizer anyway — the one outcome the RSVP verb must never produce.
    let base = base_event();
    let quiet =
        build_rsvp(&EventRsvp::to(&base, "info@example.com", RsvpResponse::Declined).quietly());
    assert_eq!(quiet["sendResponse"], false);
    assert!(quiet.get("comment").is_none());

    let loud = build_rsvp(
        &EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted).comment("See you there"),
    );
    assert_eq!(loud["sendResponse"], true);
    assert_eq!(loud["comment"], "See you there");
}

#[tokio::test]
async fn each_answer_posts_to_its_own_action_endpoint() {
    // The action segment *is* the answer on Graph — there is no status field in the body.
    // Each case routes only its own path, so posting to the wrong one 404s rather than
    // passing.
    for (response, action) in [
        (RsvpResponse::Accepted, "accept"),
        (RsvpResponse::Tentative, "tentativelyAccept"),
        (RsvpResponse::Declined, "decline"),
    ] {
        let base = base_event();
        let client = fake_client_fallible(vec![(
            Box::leak(format!("/events/evt-1/{action}").into_boxed_str()),
            Ok(sjson!(null)),
        )]);
        let receipt = rsvp_event(
            &client,
            &base,
            &EventRsvp::to(&base, "info@example.com", response),
        )
        .await
        .unwrap();

        // `202 Accepted` carries no body, so the receipt echoes the base and reports no new
        // revision — the post-write reconcile is what re-reads the event.
        assert_eq!(receipt.event, base.id);
        assert_eq!(receipt.uid, base.uid);
        assert!(receipt.revisions.etag.is_none());
    }
}

#[tokio::test]
async fn an_rsvp_to_an_event_that_is_gone_is_an_error_not_a_silent_success() {
    let base = base_event();
    let client = fake_client_fallible(vec![(
        "/events/evt-1/accept",
        Err((404, sjson!({ "error": { "code": "ErrorItemNotFound" } }))),
    )]);
    let err = rsvp_event(
        &client,
        &base,
        &EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted),
    )
    .await
    .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

// ---------------------------------------------------------------------------
// Recurrence on create
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

#[test]
fn build_create_writes_a_patterned_recurrence_not_an_rrule() {
    // Graph is the one transport that takes recurrence as a named pattern, so nothing
    // here goes through `format_rrule`.
    let body = build_create(&draft().repeating(DraftRecurrence::new(weekly_on_monday()))).unwrap();
    assert_eq!(body["recurrence"]["pattern"]["type"], "weekly");
    assert_eq!(
        body["recurrence"]["pattern"]["daysOfWeek"],
        serde_json::json!(["monday"])
    );
    assert_eq!(body["recurrence"]["range"]["type"], "noEnd");
    // The range is anchored on the draft's own start date.
    assert_eq!(body["recurrence"]["range"]["startDate"], "2026-08-03");
    assert!(body.get("recurrence").is_some());
}

#[test]
fn build_create_omits_recurrence_for_a_one_off() {
    assert!(build_create(&draft()).unwrap().get("recurrence").is_none());
}

#[test]
fn build_create_maps_a_bounded_rule_onto_graphs_range() {
    let mut counted = weekly_on_monday();
    counted.bound =
        engine_core::calendar::RecurrenceBound::Count(core::num::NonZeroU32::new(6).unwrap());
    let body = build_create(&draft().repeating(DraftRecurrence::new(counted))).unwrap();
    assert_eq!(body["recurrence"]["range"]["type"], "numbered");
    assert_eq!(body["recurrence"]["range"]["numberOfOccurrences"], 6);

    let mut until = weekly_on_monday();
    until.bound =
        engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    let body = build_create(&draft().repeating(DraftRecurrence::new(until))).unwrap();
    assert_eq!(body["recurrence"]["range"]["type"], "endDate");
    assert_eq!(body["recurrence"]["range"]["endDate"], "2026-10-26");
}

#[test]
fn a_zoned_until_needs_no_resolved_instant_on_graph() {
    // Graph's `endDate` is a plain date, so unlike CalDAV and Google this adapter never
    // needs `DraftRecurrence::until` — the same draft that CalDAV would refuse works here.
    let mut until = weekly_on_monday();
    until.bound =
        engine_core::calendar::RecurrenceBound::Until("2026-10-26T23:59:59".parse().unwrap());
    assert!(build_create(&draft().repeating(DraftRecurrence::new(until))).is_ok());
}

#[test]
fn build_create_refuses_a_rule_graph_cannot_express() {
    // The renderer's refusals reach the create path rather than being approximated into
    // a different series. `cal_recur_render` owns the full set; this is the wiring.
    let mut by_set_pos = weekly_on_monday();
    by_set_pos.by_set_position = vec![-1];
    let err = build_create(&draft().repeating(DraftRecurrence::new(by_set_pos))).unwrap_err();
    assert!(err.to_string().contains("BYSETPOS"), "{err}");
}

#[test]
fn build_patch_sets_and_clears_the_recurrence() {
    // Graph takes the structured pattern to set a rule and `null` to remove one; either
    // way the server does the surgery.
    let set = build_patch(
        &base_event(),
        &EventPatch::new(stamp()).recurrence(DraftRecurrence::new(weekly_on_monday())),
    )
    .unwrap();
    assert_eq!(set["recurrence"]["pattern"]["type"], "weekly");

    let cleared = build_patch(&base_event(), &EventPatch::new(stamp()).clear_recurrence()).unwrap();
    assert!(
        cleared["recurrence"].is_null(),
        "null turns a series into one event"
    );
}

#[test]
fn a_patch_that_does_not_mention_recurrence_leaves_it_alone() {
    // The third state: absent is not the same as cleared.
    let body = build_patch(&base_event(), &EventPatch::new(stamp()).summary("Renamed")).unwrap();
    assert!(body.get("recurrence").is_none());
}

#[test]
fn a_recurrence_change_is_significant() {
    // It moves when the meeting happens, so attendees must be told (RFC 5546 §3.2.8).
    assert!(
        EventPatch::new(stamp())
            .recurrence(DraftRecurrence::new(weekly_on_monday()))
            .is_significant()
    );
    assert!(EventPatch::new(stamp()).clear_recurrence().is_significant());
}
