//! Projecting an [`Event`] into its search-index rows.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{FtsField, FtsRow, MembershipKind, MembershipRow, normalize_addr};
use crate::calendar::{Event, ParticipantRole, ParticipationStatus};
use crate::ids::ProviderKey;

/// Which participant axis a participant-junction row indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParticipantField {
    /// An attendee — a participant with an attending role (`attendee`, `optional`,
    /// or `chair`). Matched by the DSL `attendee:` operator.
    Attendee,
    /// The organizer — the participant with the JSCalendar `owner` role. Matched by
    /// the DSL `organizer:` operator.
    Organizer,
}

/// A participant-junction row (the `event_participant` table): one address acting
/// in one role on one event. A participant who is both owner and attendee yields
/// one row per axis. `addr` is normalized (trimmed, lowercased).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventParticipantRow {
    /// The event this participant belongs to.
    pub key: ProviderKey,
    /// The participant axis.
    pub field: ParticipantField,
    /// The normalized participant address.
    pub addr: String,
    /// The participant's response status.
    pub partstat: ParticipationStatus,
}

/// The scalar index row for one event (the `event_index` table).
///
/// `my_partstat` is the response status of the participant whose address matches
/// the account's own [`OwnerAddresses`], or `None` when the account owner is not a
/// participant. The DSL `rsvp:` filter matches against it, so `rsvp:` means "how I
/// responded", not "how anyone responded".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventIndexRow {
    /// The event.
    pub key: ProviderKey,
    /// Whether the event has at least one virtual location (conference link).
    pub has_conference: bool,
    /// The account owner's participation status, if the owner is a participant.
    pub my_partstat: Option<ParticipationStatus>,
}

/// The set of addresses that identify the account owner.
///
/// Identity is per account, so each account supplies its own addresses; this is
/// how a single engine instance hosting several accounts (including several of the
/// same provider) resolves "my" RSVP independently for each. Addresses are
/// normalized (trimmed, lowercased) on construction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnerAddresses {
    addresses: BTreeSet<String>,
}

impl OwnerAddresses {
    /// Builds the owner identity from the account's addresses (aliases included).
    pub fn new(addresses: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            addresses: addresses
                .into_iter()
                .map(|a| normalize_addr(&a.into()))
                .filter(|a| !a.is_empty())
                .collect(),
        }
    }

    /// Returns `true` if no owner address is known (so no `my_partstat` is set).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    fn contains(&self, addr: &str) -> bool {
        self.addresses.contains(addr)
    }
}

/// All search-index rows derived from one calendar event.
///
/// Note this does **not** include the `OccurrenceRow`s used for `before:`/`after:`
/// time-range search: expanding recurrence to UTC instants needs tzdata and is a
/// separate step (`calendar-semantics.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventProjection {
    /// The full-text document (`subject` = title, `body` = description,
    /// `location`).
    pub fts: FtsRow,
    /// The scalar filter row.
    pub index: EventIndexRow,
    /// The attendee/organizer participant-junction rows.
    pub participants: Vec<EventParticipantRow>,
    /// The calendar membership rows.
    pub memberships: Vec<MembershipRow>,
}

/// Projects a normalized [`Event`] into its search-index rows, resolving "my" RSVP
/// against `owner`.
#[must_use]
pub fn project_event(event: &Event, owner: &OwnerAddresses) -> EventProjection {
    let key = event.id.key().clone();

    let mut fields = Vec::new();
    if !event.title.is_empty() {
        fields.push(FtsField::new("subject", &event.title));
    }
    if let Some(description) = &event.description
        && !description.is_empty()
    {
        fields.push(FtsField::new("body", description));
    }
    let location = location_text(event);
    if !location.is_empty() {
        fields.push(FtsField::new("location", location));
    }

    let mut participants = Vec::new();
    let mut my_partstat = None;
    for participant in &event.participants {
        let Some(email) = &participant.email else {
            continue;
        };
        let addr = normalize_addr(email);
        if addr.is_empty() {
            continue;
        }
        if owner.contains(&addr) {
            my_partstat = Some(participant.participation_status.clone());
        }
        for field in participant_fields(participant) {
            participants.push(EventParticipantRow {
                key: key.clone(),
                field,
                addr: addr.clone(),
                partstat: participant.participation_status.clone(),
            });
        }
    }

    let memberships = event
        .calendars
        .iter()
        .map(|calendar| MembershipRow {
            key: key.clone(),
            kind: MembershipKind::Calendar,
            value: calendar.as_str().to_owned(),
        })
        .collect();

    EventProjection {
        fts: FtsRow::new(key.clone(), fields),
        index: EventIndexRow {
            key: key.clone(),
            has_conference: !event.virtual_locations.is_empty(),
            my_partstat,
        },
        participants,
        memberships,
    }
}

/// The participant axes a participant qualifies for: organizer (the `owner` role)
/// and/or attendee (an attending role). A participant with neither is not indexed.
fn participant_fields(
    participant: &crate::calendar::Participant,
) -> impl Iterator<Item = ParticipantField> {
    let organizer = participant.has_role(&ParticipantRole::Owner);
    let attendee = participant.has_role(&ParticipantRole::Attendee)
        || participant.has_role(&ParticipantRole::Optional)
        || participant.has_role(&ParticipantRole::Chair);
    [
        organizer.then_some(ParticipantField::Organizer),
        attendee.then_some(ParticipantField::Attendee),
    ]
    .into_iter()
    .flatten()
}

/// The searchable location text: each physical location's name and description.
fn location_text(event: &Event) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for location in &event.locations {
        if let Some(name) = &location.name {
            parts.push(name);
        }
        if let Some(description) = &location.description {
            parts.push(description);
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{Location, Participant, VirtualLocation};
    use crate::ids::{CalendarId, EventId, Uid};
    use crate::membership::Memberships;
    use crate::time::{CalendarDateTime, LocalDateTime, TimeZoneId};

    fn event() -> Event {
        Event::new(
            EventId::try_from("evt-1").unwrap(),
            Uid::new("uid-1").unwrap(),
            Memberships::of_one(CalendarId::try_from("work").unwrap()),
            CalendarDateTime::Zoned {
                local: LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap(),
                zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            },
        )
    }

    fn owner() -> OwnerAddresses {
        OwnerAddresses::new(["Me@Example.com"])
    }

    #[test]
    fn projects_text_membership_and_conference() {
        let mut ev = event();
        ev.title = "Standup".into();
        ev.description = Some("Daily sync".into());
        let mut room = Location::named("Room 4");
        room.description = Some("3rd floor".into());
        ev.locations = vec![room];
        ev.virtual_locations = vec![VirtualLocation::new("https://meet.example/x")];

        let p = project_event(&ev, &owner());
        assert_eq!(
            p.fts.fields,
            vec![
                FtsField::new("subject", "Standup"),
                FtsField::new("body", "Daily sync"),
                // The location name and description are both indexed, space-joined.
                FtsField::new("location", "Room 4 3rd floor"),
            ]
        );
        assert!(p.index.has_conference);
        assert_eq!(
            p.memberships,
            vec![MembershipRow {
                key: p.fts.key.clone(),
                kind: MembershipKind::Calendar,
                value: "work".into(),
            }]
        );
        assert_eq!(p.index.my_partstat, None);
    }

    #[test]
    fn organizer_and_attendee_are_distinct_rows_and_my_rsvp_resolves() {
        let mut ev = event();
        let mut organizer = Participant::attendee("Me@Example.com");
        organizer.roles.insert(ParticipantRole::Owner);
        organizer.participation_status = ParticipationStatus::Accepted;
        let attendee = Participant::attendee("guest@example.com");
        ev.participants = vec![organizer, attendee];

        let p = project_event(&ev, &owner());
        // The owner has both roles → an organizer and an attendee row.
        assert!(p.participants.contains(&EventParticipantRow {
            key: p.fts.key.clone(),
            field: ParticipantField::Organizer,
            addr: "me@example.com".into(),
            partstat: ParticipationStatus::Accepted,
        }));
        assert!(p.participants.contains(&EventParticipantRow {
            key: p.fts.key.clone(),
            field: ParticipantField::Attendee,
            addr: "me@example.com".into(),
            partstat: ParticipationStatus::Accepted,
        }));
        // The guest is attendee-only.
        assert!(p.participants.contains(&EventParticipantRow {
            key: p.fts.key.clone(),
            field: ParticipantField::Attendee,
            addr: "guest@example.com".into(),
            partstat: ParticipationStatus::NeedsAction,
        }));
        // "My" RSVP resolves from the owner-matching participant.
        assert_eq!(p.index.my_partstat, Some(ParticipationStatus::Accepted));
    }

    #[test]
    fn participant_without_email_or_attending_role_is_skipped() {
        let mut ev = event();
        let mut emailless = Participant::attendee("x@example.com");
        emailless.email = None;
        let mut informational = Participant::attendee("info@example.com");
        informational.roles.clear();
        informational.roles.insert(ParticipantRole::Informational);
        // A whitespace-only address normalizes to empty and is dropped too.
        let blank = Participant::attendee("   ");
        ev.participants = vec![emailless, informational, blank];
        let p = project_event(&ev, &owner());
        assert!(p.participants.is_empty());
    }

    #[test]
    fn owner_addresses_normalize_and_report_empty() {
        assert!(OwnerAddresses::new(Vec::<String>::new()).is_empty());
        assert!(OwnerAddresses::new(["   "]).is_empty());
        // A non-owner participant leaves my_partstat unset.
        let mut ev = event();
        ev.participants = vec![Participant::attendee("someone@else.com")];
        assert_eq!(project_event(&ev, &owner()).index.my_partstat, None);
    }
}
