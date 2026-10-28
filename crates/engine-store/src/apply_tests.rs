//! [`ApplyBatch`](super::ApplyBatch) and [`DerivedWrite`](super::DerivedWrite) assembly.

use engine_core::{
    ids::{EventId, MailboxId, MessageId, Uid},
    mail::{EmailAddress, Message},
    membership::Memberships,
    search_index::{MembershipKind, OwnerAddresses, project_event, project_message},
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};

use super::*;

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

fn message(id: &str) -> Message {
    Message::new(
        MessageId::try_from(id).unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    )
}

#[test]
fn derived_write_emptiness_tracks_every_row_kind() {
    let mut d = DerivedWrite::empty();
    assert!(d.is_empty());
    d.fts
        .push(FtsRow::new(key("m1"), vec![FtsField::new("subject", "hi")]));
    assert!(!d.is_empty());

    let json = serde_json::to_string(&d).unwrap();
    assert_eq!(serde_json::from_str::<DerivedWrite>(&json).unwrap(), d);
}

#[test]
fn push_mail_flattens_a_projection_into_the_row_lists() {
    let mut msg = Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
    );
    msg.envelope.subject = Some("hello".into());
    msg.envelope.from = vec![EmailAddress::new("a@example.com")];

    let mut d = DerivedWrite::empty();
    d.push_mail(project_message(&msg));
    assert_eq!(d.fts.len(), 1);
    assert_eq!(d.messages.len(), 1);
    assert_eq!(d.addresses.len(), 1);
    assert_eq!(d.memberships.len(), 1);
    assert_eq!(d.memberships[0].kind, MembershipKind::Mailbox);
}

#[test]
fn push_event_flattens_a_projection_into_the_row_lists() {
    let event = engine_core::calendar::Event::new(
        EventId::try_from("e1").unwrap(),
        Uid::new("uid-1").unwrap(),
        Memberships::of_one(engine_core::ids::CalendarId::try_from("work").unwrap()),
        CalendarDateTime::Zoned {
            local: LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        },
    );
    let mut d = DerivedWrite::empty();
    d.push_event(project_event(&event, &OwnerAddresses::default()));
    assert_eq!(d.event_index.len(), 1);
    assert_eq!(d.memberships.len(), 1);
    assert_eq!(d.memberships[0].kind, MembershipKind::Calendar);
}

#[test]
fn occurrence_row_roundtrips_with_optional_override() {
    let occ = OccurrenceRow {
        event: key("evt-1"),
        start: "2026-03-01T09:00:00Z".parse().unwrap(),
        end: "2026-03-01T10:00:00Z".parse().unwrap(),
        recurrence_id: Some("2026-03-01T09:00:00Z".parse().unwrap()),
        tzdata_version: TzdataVersion::new("2025b"),
    };
    let json = serde_json::to_string(&occ).unwrap();
    assert_eq!(serde_json::from_str::<OccurrenceRow>(&json).unwrap(), occ);
    assert_eq!(occ.tzdata_version.as_str(), "2025b");
}

#[test]
fn reconciliation_carries_expected_state_and_target() {
    let rec = PendingReconciliation::new(PendingOpId::new(3), PendingOpState::InFlight, key("m9"));
    assert_eq!(rec.op, PendingOpId::new(3));
    assert_eq!(rec.expected, PendingOpState::InFlight);
    assert_eq!(rec.resolves_to, key("m9"));
    let json = serde_json::to_string(&rec).unwrap();
    assert_eq!(
        serde_json::from_str::<PendingReconciliation>(&json).unwrap(),
        rec
    );
}

#[test]
fn apply_batch_borrows_its_parts() {
    let update: SyncUpdate<Message> = SyncUpdate::delta(vec![message("m1")], vec![]);
    let derived = DerivedWrite::empty();
    let recs: Vec<PendingReconciliation> = Vec::new();
    let next = SyncState::new("cursor-2");
    let batch = ApplyBatch::new(&update, &derived, &recs, &next);
    assert!(batch.derived.is_empty());
    assert_eq!(batch.next_state.unwrap().as_str(), "cursor-2");
    assert_eq!(batch.reconcile, []);

    // A streaming page leaves the cursor unchanged.
    let held = ApplyBatch::with_cursor(&update, &derived, &recs, None);
    assert!(held.next_state.is_none());
}
