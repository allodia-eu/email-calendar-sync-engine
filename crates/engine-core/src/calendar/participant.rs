//! Event participants.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

open_enum! {
    /// A participant's response status (JSCalendar `participationStatus`,
    /// RFC 8984 §4.4.6; iCalendar `PARTSTAT`). Defaults to `needs-action`.
    ParticipationStatus {
        /// Has not yet responded.
        NeedsAction => "needs-action",
        /// Accepted the invitation.
        Accepted => "accepted",
        /// Declined the invitation.
        Declined => "declined",
        /// Tentatively accepted.
        Tentative => "tentative",
        /// Delegated to another participant.
        Delegated => "delegated",
    }
}

open_enum! {
    /// A participant's role (JSCalendar `roles`, RFC 8984 §4.4.6; iCalendar
    /// `ROLE`). A participant has one or more.
    ParticipantRole {
        /// Owns the event (the organizer's role in JSCalendar).
        Owner => "owner",
        /// A required attendee.
        Attendee => "attendee",
        /// An optional attendee.
        Optional => "optional",
        /// Included for information only.
        Informational => "informational",
        /// Chairs/runs the event.
        Chair => "chair",
        /// A contact for the event.
        Contact => "contact",
    }
}

open_enum! {
    /// What kind of entity a participant is (JSCalendar `kind`, RFC 8984
    /// §4.4.6; iCalendar `CUTYPE`).
    ParticipantKind {
        /// A single person.
        Individual => "individual",
        /// A group/distribution list.
        Group => "group",
        /// A physical resource.
        Resource => "resource",
        /// A location resource.
        Location => "location",
    }
}

/// A participant in an event (JSCalendar `Participant`, RFC 8984 §4.4.6).
///
/// The full delegation graph (`delegatedTo`/`delegatedFrom`/`memberOf`) and the
/// JSCalendar participant-id map keys are not modeled in this slice; RSVP and
/// scheduling reconcile by `email`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    /// The display name, if any.
    pub name: Option<String>,
    /// The participant's address (the iTIP `ATTENDEE`/`ORGANIZER` cal-address).
    pub email: Option<String>,
    /// What kind of entity this participant is.
    pub kind: Option<ParticipantKind>,
    /// The participant's roles (at least one in a well-formed event).
    pub roles: BTreeSet<ParticipantRole>,
    /// The response status.
    pub participation_status: ParticipationStatus,
    /// Whether a reply is expected from this participant.
    pub expect_reply: bool,
    /// A free-text comment from the participant's reply.
    pub comment: Option<String>,
    /// The address acting on this participant's behalf, if delegated/sent-by.
    pub sent_by: Option<String>,
}

impl Participant {
    /// Creates an attendee with the given address, the `attendee` role, and the
    /// default `needs-action` status.
    #[must_use]
    pub fn attendee(email: impl Into<String>) -> Self {
        let mut roles = BTreeSet::new();
        roles.insert(ParticipantRole::Attendee);
        Self {
            name: None,
            email: Some(email.into()),
            kind: None,
            roles,
            participation_status: ParticipationStatus::NeedsAction,
            expect_reply: true,
            comment: None,
            sent_by: None,
        }
    }

    /// Returns `true` if the participant has the given role.
    #[must_use]
    pub fn has_role(&self, role: &ParticipantRole) -> bool {
        self.roles.contains(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_enums_preserve_unknown_values() {
        // JSCalendar requires unknown enum values be preserved.
        let status = ParticipationStatus::from_wire("snoozed");
        assert_eq!(status, ParticipationStatus::Other("snoozed".into()));
        assert_eq!(status.as_str(), "snoozed");
        assert_eq!(
            serde_json::from_str::<ParticipantRole>("\"sponsor\"").unwrap(),
            ParticipantRole::Other("sponsor".into())
        );
    }

    #[test]
    fn participation_status_wire_strings() {
        assert_eq!(ParticipationStatus::NeedsAction.as_str(), "needs-action");
        assert_eq!(
            serde_json::to_string(&ParticipationStatus::Accepted).unwrap(),
            "\"accepted\""
        );
    }

    #[test]
    fn attendee_defaults_and_roundtrip() {
        let mut p = Participant::attendee("a@example.com");
        assert!(p.has_role(&ParticipantRole::Attendee));
        assert_eq!(p.participation_status, ParticipationStatus::NeedsAction);
        p.participation_status = ParticipationStatus::Accepted;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Participant>(&json).unwrap(), p);
    }
}
