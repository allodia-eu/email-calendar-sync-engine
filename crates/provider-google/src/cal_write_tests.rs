//! Offline tests for calendar writes: the create/patch bodies, the form guard, the
//! create/patch/delete flows over the fake, and the exact request shapes over the
//! capturing server (which the fakes cannot assert — `AGENTS.md`).

use engine_core::{
    calendar::Event,
    error::FailureClass,
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDate, CalendarDateTime, LocalDateTime, TimeZoneId, UtcDateTime},
    version::{ETag, RevisionTokens},
};
use engine_provider::{EventDeletion, EventDraft, EventEdit, EventPatch, PatchTarget};
use serde_json::json as sjson;

use super::*;
use crate::{
    GoogleClient,
    test_support::{capturing_server, fake_client_fallible, tls},
};

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
    event.revisions = RevisionTokens::from_etag(ETag::new("\"v7\""));
    event
}

/// A Google created/updated event response.
fn stored(id: &str, ical_uid: &str, etag: &str) -> serde_json::Value {
    sjson!({ "id": id, "iCalUID": ical_uid, "etag": etag, "status": "confirmed" })
}

#[test]
fn build_create_maps_summary_zone_location_and_description() {
    let body = build_create(&draft()).unwrap();
    assert_eq!(body["summary"], "Sprint planning");
    // A zoneless dateTime paired with the IANA timeZone (Google interprets it there).
    assert_eq!(body["start"]["dateTime"], "2026-08-03T09:00:00");
    assert_eq!(body["start"]["timeZone"], "Europe/Amsterdam");
    assert_eq!(body["end"]["dateTime"], "2026-08-03T09:30:00");
    // Google's location and description are plain strings (not objects).
    assert_eq!(body["location"], "Room A");
    assert_eq!(body["description"], "agenda");
}

#[test]
fn build_create_marks_an_all_day_event_with_a_date() {
    let draft = EventDraft::new(
        calendar(),
        Uid::new("u@test.local").unwrap(),
        "Offsite",
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 10).unwrap()),
        CalendarDateTime::Date(CalendarDate::new(2026, 8, 11).unwrap()),
        stamp(),
    );
    let body = build_create(&draft).unwrap();
    // An all-day event is `{ date }`, not `{ dateTime, timeZone }`.
    assert_eq!(body["start"]["date"], "2026-08-10");
    assert!(body["start"].get("dateTime").is_none());
}

#[test]
fn build_create_rejects_a_floating_start() {
    let draft = EventDraft::new(
        calendar(),
        Uid::new("u@test.local").unwrap(),
        "floating",
        CalendarDateTime::Floating("2026-08-03T09:00:00".parse().unwrap()),
        CalendarDateTime::Floating("2026-08-03T10:00:00".parse().unwrap()),
        stamp(),
    );
    assert_eq!(
        build_create(&draft).unwrap_err().class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn create_event_returns_the_server_id_uid_and_etag() {
    let client = fake_client_fallible(vec![(
        "/events",
        Ok(stored("srv-id", "server-uid@google.com", "\"v1\"")),
    )]);
    let receipt = create_event(&client, "cal-1", &draft()).await.unwrap();
    assert_eq!(receipt.event.key().as_str(), "srv-id");
    assert_eq!(receipt.uid.as_str(), "server-uid@google.com");
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v1\"")));
}

#[test]
fn build_patch_sends_only_changed_fields_and_keeps_the_zone() {
    let patch = EventPatch::new(stamp())
        .summary("Renamed")
        .start(zoned("2026-08-03T10:00:00"));
    let body = build_patch(&base_event(), &patch).unwrap();
    assert_eq!(body["summary"], "Renamed");
    assert_eq!(body["start"]["dateTime"], "2026-08-03T10:00:00");
    assert_eq!(body["start"]["timeZone"], "Europe/Amsterdam");
    assert!(body.get("description").is_none() && body.get("location").is_none());
}

#[test]
fn build_patch_clears_text_and_moves_the_end() {
    let patch = EventPatch::new(stamp())
        .clear_description()
        .clear_location()
        .end(zoned("2026-08-03T11:00:00"));
    let body = build_patch(&base_event(), &patch).unwrap();
    assert_eq!(body["description"], "");
    assert_eq!(body["location"], "");
    assert_eq!(body["end"]["dateTime"], "2026-08-03T11:00:00");
    assert!(body.get("summary").is_none());
}

#[test]
fn build_patch_rejects_a_form_change() {
    // Moving a zoned event to a UTC instant is silent corruption — refused.
    let patch = EventPatch::new(stamp()).start(CalendarDateTime::utc(
        "2026-08-03T10:00:00".parse().unwrap(),
    ));
    assert_eq!(
        build_patch(&base_event(), &patch).unwrap_err().class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn patch_event_empty_is_a_no_op_and_a_per_occurrence_target_is_refused() {
    let client = fake_client_fallible(vec![]);
    // An empty patch neither errors nor calls the server; it reports the base revision.
    let edit = EventEdit::new(&base_event(), PatchTarget::Series, EventPatch::new(stamp()));
    let receipt = patch_event(&client, "cal-1", &base_event(), &edit)
        .await
        .unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v7\"")));
    // A per-occurrence edit is deferred → refused.
    let per_occ = EventEdit::new(
        &base_event(),
        PatchTarget::Instance(zoned("2026-08-10T09:00:00")),
        EventPatch::new(stamp()).summary("x"),
    );
    assert_eq!(
        patch_event(&client, "cal-1", &base_event(), &per_occ)
            .await
            .unwrap_err()
            .class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn patch_event_returns_the_new_etag_or_falls_back_to_the_base() {
    let client = fake_client_fallible(vec![(
        "/events/evt-1",
        Ok(stored("evt-1", "evt-1@test.local", "\"v8\"")),
    )]);
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    let receipt = patch_event(&client, "cal-1", &base_event(), &edit)
        .await
        .unwrap();
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v8\"")));

    // A no-echoed-body patch still resolves to the base identity.
    let none = fake_client_fallible(vec![("/events/evt-1", Ok(serde_json::Value::Null))]);
    let receipt = patch_event(&none, "cal-1", &base_event(), &edit)
        .await
        .unwrap();
    assert_eq!(receipt.event.as_str(), "evt-1");
    assert!(receipt.revisions.etag.is_none());
}

#[tokio::test]
async fn delete_event_is_idempotent_on_404_and_410_but_a_conflict_on_412() {
    let deletion = EventDeletion::of(&base_event());
    // Google signals "already gone" as 404 or 410 (a re-delete of a deleted event).
    let gone = fake_client_fallible(vec![("/events/evt-1", Err((404, sjson!({}))))]);
    assert!(delete_event(&gone, "cal-1", &deletion).await.is_ok());
    let already = fake_client_fallible(vec![("/events/evt-1", Err((410, sjson!({}))))]);
    assert!(delete_event(&already, "cal-1", &deletion).await.is_ok());
    // A stale If-Match on an event that still exists → a conflict (refetch, then retry).
    let stale = fake_client_fallible(vec![("/events/evt-1", Err((412, sjson!({}))))]);
    assert_eq!(
        delete_event(&stale, "cal-1", &deletion)
            .await
            .unwrap_err()
            .class(),
        FailureClass::Conflict
    );
    let ok = fake_client_fallible(vec![("/events/evt-1", Ok(serde_json::Value::Null))]);
    assert!(delete_event(&ok, "cal-1", &deletion).await.is_ok());
}

#[tokio::test]
async fn create_posts_the_event_json_over_the_real_transport() {
    let (base, rx) = capturing_server("200 OK", &stored("x", "u@google.com", "\"v1\"").to_string());
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    create_event(&client, "cal-1", &draft()).await.unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("POST /calendar/v3/calendars/cal-1/events "),
        "{request}"
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["summary"], "Sprint planning");
    assert_eq!(json["start"]["timeZone"], "Europe/Amsterdam");
}

#[tokio::test]
async fn patch_sends_if_match_and_only_the_changed_field() {
    let (base, rx) = capturing_server(
        "200 OK",
        &stored("evt-1", "u@google.com", "\"v8\"").to_string(),
    );
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    let edit = EventEdit::new(
        &base_event(),
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    );
    patch_event(&client, "cal-1", &base_event(), &edit)
        .await
        .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("PATCH /calendar/v3/calendars/cal-1/events/evt-1 "),
        "{request}"
    );
    // The If-Match precondition carries the base's ETag (the lost-update guard).
    assert!(
        request.contains("if-match: \"v7\"")
            || request.to_ascii_lowercase().contains("if-match: \"v7\"")
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["summary"], "Renamed");
    assert!(json.get("start").is_none());
}
