//! Unit tests for the outbound calendar write shapes: what each request carries, which
//! precondition it asks for, and that every one survives the durable outbox payload. A
//! sibling file so `mod.rs` stays under the line limit.

use engine_core::{
    ids::ProviderKey,
    membership::Memberships,
    time::{LocalDateTime, TimeZoneId},
    version::ETag,
};

use super::*;

fn event_id() -> EventId {
    EventId::try_from("/dav/cal/alice/default/evt-1.ics").unwrap()
}

fn uid() -> Uid {
    Uid::new("evt-1@test.local").unwrap()
}

fn calendar() -> CalendarId {
    CalendarId::new(ProviderKey::new("/dav/cal/alice/default/").unwrap())
}

fn zoned(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse::<LocalDateTime>().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

/// An event as a sync hands it back: id, uid, and the revision it was read at.
fn stored(revisions: RevisionTokens) -> Event {
    let mut event = Event::new(
        event_id(),
        uid(),
        Memberships::of_one(calendar()),
        zoned("2026-08-01T09:00:00"),
    );
    event.revisions = revisions;
    event
}

#[test]
fn removing_one_occurrence_still_names_the_series() {
    // The id is the *series'*, never a synthetic one for the instance: the two transports
    // that have such an id derive it differently, so deriving it is the adapter's job.
    let base = stored(RevisionTokens::from_etag(ETag::new("\"v7\"")));
    let deletion = EventDeletion::occurrence(
        &base,
        Occurrence::starting(zoned("2026-08-08T09:00:00")),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    );

    assert_eq!(deletion.event, event_id());
    assert_eq!(deletion.uid, uid());
    assert_eq!(
        deletion.occurrence_target().map(|o| o.start.clone()),
        Some(zoned("2026-08-08T09:00:00"))
    );
    assert!(
        EventDeletion::of(&base).occurrence_target().is_none(),
        "a whole-event delete targets no occurrence"
    );
}

#[test]
fn only_a_caller_who_resolved_the_instant_carries_one() {
    // Google names an occurrence by its start in UTC and no adapter carries tzdata, so the
    // resolution is the caller's — and its absence has to be visible rather than guessed at.
    let start = zoned("2026-08-08T09:00:00");
    assert!(Occurrence::starting(start.clone()).instant.is_none());
    assert_eq!(
        Occurrence::at(start, "2026-08-08T07:00:00Z".parse().unwrap()).instant,
        Some("2026-08-08T07:00:00Z".parse().unwrap())
    );
}

#[test]
fn a_draft_carries_intent_but_never_an_id() {
    // The caller mints the UID — the cross-system identity, and what makes a retried
    // create recognizable — but never the EventId: a server-assigning transport hands
    // that back on the receipt.
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    )
    .description("agenda")
    .location("Room A");
    assert_eq!(draft.uid, uid());
    assert_eq!(draft.description.as_deref(), Some("agenda"));
    assert_eq!(draft.location.as_deref(), Some("Room A"));
}

#[test]
fn a_draft_has_no_location_until_one_is_given() {
    // A create is the one write that can set a location from nothing; without the
    // builder it carries none, which the adapters render as no LOCATION at all.
    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    );
    assert!(draft.location.is_none());
}

#[test]
fn a_delete_is_guarded_by_the_revision_the_caller_read() {
    let base = stored(RevisionTokens::from_etag(ETag::new("\"v7\"")));
    let guarded = EventDeletion::of(&base);
    assert_eq!(guarded.event, event_id());
    assert_eq!(
        guarded.guard.unwrap().etag,
        Some(ETag::new("\"v7\"")),
        "the guard must come from the event as read, never be hand-assembled"
    );
    assert!(
        EventDeletion::unconditional(event_id(), uid())
            .guard
            .is_none()
    );
}

#[test]
fn a_document_write_guards_on_the_event_it_replaces() {
    // The iMIP RSVP path: patch my PARTSTAT into the stored raw, then replace the
    // document under the revision I read it at.
    let base = stored(RevisionTokens::from_etag(ETag::new("\"v7\"")));
    let write = EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"));
    assert_eq!(write.event, event_id());
    assert_eq!(write.uid, uid());
    let WritePrecondition::IfUnchanged(tokens) = write.guard else {
        panic!("a replace guards on the revision it read");
    };
    assert_eq!(tokens.etag, Some(ETag::new("\"v7\"")));
}

#[test]
fn a_document_write_can_ask_to_create_rather_than_replace() {
    // Storing an invitation that arrived as mail is a *create*, and its precondition is
    // the opposite of an update's. Without this state the caller's only options are a
    // guard naming a revision it never read and an unconditional write — and the
    // unconditional one silently overwrites the copy the server scheduled a moment ago.
    let write = EventWrite::creating(
        event_id(),
        uid(),
        RawIcal::new("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR"),
    );
    assert_eq!(write.guard, WritePrecondition::IfAbsent);
    assert_ne!(
        write.guard,
        WritePrecondition::Unconditional,
        "a create is not an unguarded write; conflating them is the overwrite this exists \
         to prevent"
    );
}

#[test]
fn the_three_preconditions_stay_distinct_when_the_caller_read_no_revision() {
    // JMAP objects carry no revision token at all. A replace built from one still *asks*
    // for a guard — it just names an empty revision, which no transport can enforce.
    // That is neither a waived guard nor a create, and all three must stay tellable
    // apart: `Capabilities::calendar_write_guard` is what says which one a host got.
    let base = stored(RevisionTokens::none());
    let ical = RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR");
    let guarded = EventWrite::replacing(&base, ical.clone());
    let WritePrecondition::IfUnchanged(tokens) = &guarded.guard else {
        panic!("a replace asks for a guard even when the revision is empty");
    };
    assert!(tokens.is_empty());

    assert_eq!(
        EventWrite::unconditional(event_id(), uid(), ical.clone()).guard,
        WritePrecondition::Unconditional
    );
    assert_eq!(
        EventWrite::creating(event_id(), uid(), ical).guard,
        WritePrecondition::IfAbsent
    );
}

#[test]
fn asking_for_a_guard_and_waiving_one_stay_distinguishable_with_no_tokens() {
    // JMAP objects carry no revision token at all. A write built from one still *asks*
    // for a guard — it just names an empty revision, which no transport can enforce.
    // That is not the same as deliberately waiving the guard, and the two must not
    // collapse: `Capabilities::calendar_write_guard` is what tells a host which it got.
    let base = stored(RevisionTokens::none());
    let deletion = EventDeletion::of(&base);
    assert!(deletion.guard.as_ref().unwrap().is_empty());
    assert!(
        EventDeletion::unconditional(event_id(), uid())
            .guard
            .is_none()
    );
}

#[test]
fn write_requests_survive_the_durable_payload_round_trip() {
    // Every request is stored as JSON in the outbox before the network call, so a
    // restart must read back exactly what was intended.
    let base = stored(RevisionTokens::from_etag(ETag::new("\"v7\"")));

    let deletion = EventDeletion::of(&base);
    let encoded = serde_json::to_value(&deletion).unwrap();
    assert_eq!(
        serde_json::from_value::<EventDeletion>(encoded).unwrap(),
        deletion
    );

    for write in [
        EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR")),
        EventWrite::creating(
            event_id(),
            uid(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        ),
        EventWrite::unconditional(
            event_id(),
            uid(),
            RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"),
        ),
    ] {
        let encoded = serde_json::to_value(&write).unwrap();
        assert_eq!(
            serde_json::from_value::<EventWrite>(encoded).unwrap(),
            write,
            "every precondition must survive the outbox, or a restart changes what the \
             write asks the server to check"
        );
    }

    let draft = EventDraft::new(
        calendar(),
        uid(),
        "Sprint planning",
        zoned("2026-08-01T09:00:00"),
        zoned("2026-08-01T09:30:00"),
        "2026-07-14T10:00:00Z".parse().unwrap(),
    )
    .location("Room A");
    let encoded = serde_json::to_value(&draft).unwrap();
    assert_eq!(
        serde_json::from_value::<EventDraft>(encoded).unwrap(),
        draft
    );
}

#[test]
fn a_receipt_reports_the_id_the_write_resolved_to() {
    let receipt = EventWriteReceipt::new(
        event_id(),
        uid(),
        RevisionTokens::from_etag(ETag::new("\"v8\"")),
    );
    assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v8\"")));
    assert_eq!(receipt.uid, uid());
}
