//! The **intent** of an RSVP: which address is answering, what the answer is, and what
//! should reach the organizer.
//!
//! An RSVP is not an edit of the event. It changes exactly one participant's status — *the
//! account's own* — and every transport has a first-class verb for it, because the server
//! has to do something an edit never does: tell the organizer. `PATCH`ing the attendee array
//! would change the same bytes and skip the scheduling entirely, which is the failure mode
//! this type exists to make unreachable.
//!
//! # Why the answer is not a `ParticipationStatus`
//!
//! [`ParticipationStatus`] is an open enum covering every state a participant can be *in*,
//! including two nobody can choose: `needs-action` is the *absence* of an answer, and
//! `delegated` is a different verb (it names someone else). A closed three-value
//! [`RsvpResponse`] makes "RSVP `needs-action`" unrepresentable rather than a runtime error
//! four adapters would each have to remember to raise.
//!
//! # Why the attendee address is carried, not derived
//!
//! An invitation to `info@…` on an account whose primary identity is `dennis@…` must RSVP as
//! **`info@…`** — that is the `ATTENDEE` line the server will look for, and writing the
//! primary instead either fails or silently adds a second attendee. Identity is a *set*, and
//! the caller is the only layer that knows which member of it the invitation matched. So the
//! matched address travels with the intent; an adapter never guesses it from the account.
//!
//! # Two things the transports genuinely differ on
//!
//! [`comment`](EventRsvp::comment) and [`notify_organizer`](EventRsvp::notify_organizer) are
//! Outlook's "optional message" and "Email organizer" toggle, and **not every transport has
//! them**. Read
//! [`Capabilities::calendar_rsvp`](crate::Capabilities::calendar_rsvp) before offering
//! either: an adapter that cannot honour one **refuses** the write rather than dropping it,
//! because a note that silently goes nowhere is worse than a note the user was never offered.

use engine_core::{
    calendar::{Event, ParticipationStatus},
    ids::{EventId, Uid},
    version::RevisionTokens,
};
use serde::{Deserialize, Serialize};

/// The answer a user can give to an invitation.
///
/// Three values, closed on purpose — see the module docs. Maps onto
/// [`ParticipationStatus`] on the way out ([`status`](RsvpResponse::status)); nothing maps
/// back, because an event can hold statuses no RSVP could have produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RsvpResponse {
    /// Yes. iCalendar `PARTSTAT=ACCEPTED`, JSCalendar `accepted`.
    Accepted,
    /// Maybe. iCalendar `PARTSTAT=TENTATIVE`, JSCalendar `tentative`.
    Tentative,
    /// No. iCalendar `PARTSTAT=DECLINED`, JSCalendar `declined`.
    Declined,
}

impl RsvpResponse {
    /// The participation status this answer sets.
    #[must_use]
    pub const fn status(self) -> ParticipationStatus {
        match self {
            Self::Accepted => ParticipationStatus::Accepted,
            Self::Tentative => ParticipationStatus::Tentative,
            Self::Declined => ParticipationStatus::Declined,
        }
    }
}

/// A request to answer an invitation.
///
/// Built from the event **as the caller read it**, so the write is guarded by the revision
/// it was read at — an RSVP that lands on a copy the organizer has since rescheduled is
/// refused rather than answering the wrong meeting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRsvp {
    /// The event being answered.
    pub event: EventId,
    /// Its cross-system `UID`, echoed on the receipt for reconciliation — and the key the
    /// outbox serializes calendar writes on, so an RSVP and an edit of one event never race.
    pub uid: Uid,
    /// The address that is answering: the `ATTENDEE` the invitation **matched**, which on an
    /// aliased account is not the account's primary identity. See the module docs.
    pub attendee: String,
    /// The answer.
    pub response: RsvpResponse,
    /// An optional note for the organizer.
    ///
    /// Only where
    /// [`RsvpControls::comment`](crate::RsvpControls::comment) is set: an adapter with
    /// nowhere to put it refuses the write rather than dropping the note.
    pub comment: Option<String>,
    /// Whether the organizer is told.
    ///
    /// `true` is the RFC 5546 default — an invitation asks for a reply, so answering sends
    /// one. `false` is honoured only where
    /// [`RsvpControls::suppress_notification`](crate::RsvpControls::suppress_notification)
    /// is set; on a server-scheduled transport the server emits the `REPLY` the moment it
    /// sees the changed status and the client cannot stop it, so the adapter refuses rather
    /// than pretending.
    pub notify_organizer: bool,
    /// The revision the RSVP is guarded by — the one the caller read. `None` answers
    /// unconditionally.
    pub guard: Option<RevisionTokens>,
}

impl EventRsvp {
    /// Answers `base` — the event as the caller read it — as `attendee`, guarded by the
    /// revision it was read at.
    ///
    /// `attendee` must be the address the invitation **matched**, not the account's primary
    /// identity (module docs).
    #[must_use]
    pub fn to(base: &Event, attendee: impl Into<String>, response: RsvpResponse) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            attendee: attendee.into(),
            response,
            comment: None,
            notify_organizer: true,
            guard: Some(base.revisions.clone()),
        }
    }

    /// Sends a note to the organizer along with the answer.
    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Answers **without** telling the organizer.
    #[must_use]
    pub const fn quietly(mut self) -> Self {
        self.notify_organizer = false;
        self
    }
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{CalendarId, ProviderKey},
        membership::Memberships,
        time::{CalendarDateTime, LocalDateTime, TimeZoneId},
        version::ETag,
    };

    use super::*;

    /// An event as a sync hands it back: id, uid, and the revision it was read at.
    fn stored(revisions: RevisionTokens) -> Event {
        let mut event = Event::new(
            EventId::try_from("/dav/cal/alice/default/evt-1.ics").unwrap(),
            Uid::new("evt-1@test.local").unwrap(),
            Memberships::of_one(CalendarId::new(
                ProviderKey::new("/dav/cal/alice/default/").unwrap(),
            )),
            CalendarDateTime::Zoned {
                local: "2026-08-03T09:30:00".parse::<LocalDateTime>().unwrap(),
                zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            },
        );
        event.revisions = revisions;
        event
    }

    #[test]
    fn an_rsvp_answers_as_the_address_the_invitation_matched() {
        // The whole point of D5: an alias invitation answers as the alias. If this ever
        // derived the address from the account instead, an `info@` invitation would write
        // the wrong `ATTENDEE` and either fail or add a second attendee.
        let base = stored(RevisionTokens::none());
        let rsvp = EventRsvp::to(&base, "info@example.com", RsvpResponse::Accepted);

        assert_eq!(rsvp.attendee, "info@example.com");
        assert_eq!(rsvp.event, base.id);
        assert_eq!(rsvp.uid, base.uid);
    }

    #[test]
    fn answering_tells_the_organizer_unless_asked_not_to() {
        // RFC 5546's default: an invitation asks for a reply, so answering sends one. A
        // caller has to say otherwise in as many words.
        let base = stored(RevisionTokens::none());
        let loud = EventRsvp::to(&base, "me@example.com", RsvpResponse::Declined);
        assert!(loud.notify_organizer);
        assert!(loud.comment.is_none());

        let quiet = loud.clone().quietly().comment("Clashes with the offsite");
        assert!(!quiet.notify_organizer);
        assert_eq!(quiet.comment.as_deref(), Some("Clashes with the offsite"));
    }

    #[test]
    fn an_rsvp_is_guarded_by_the_revision_the_caller_read() {
        // Answering a copy the organizer has since rescheduled must be refused, not applied
        // to whatever the server now holds.
        let tokens = RevisionTokens::from_etag(ETag::new("\"v7\""));
        let base = stored(tokens.clone());
        let rsvp = EventRsvp::to(&base, "me@example.com", RsvpResponse::Tentative);

        assert_eq!(rsvp.guard, Some(tokens));
    }

    #[test]
    fn the_three_answers_map_onto_the_statuses_they_set() {
        assert_eq!(
            RsvpResponse::Accepted.status(),
            ParticipationStatus::Accepted
        );
        assert_eq!(
            RsvpResponse::Tentative.status(),
            ParticipationStatus::Tentative
        );
        assert_eq!(
            RsvpResponse::Declined.status(),
            ParticipationStatus::Declined
        );
    }

    #[test]
    fn an_rsvp_survives_the_durable_payload_round_trip() {
        // The outbox stores the intent as JSON before the side effect; a field that did not
        // survive would be silently dropped on a crash-recovery retry.
        let base = stored(RevisionTokens::none());
        let rsvp = EventRsvp::to(&base, "info@example.com", RsvpResponse::Declined)
            .comment("Sorry, away that week")
            .quietly();

        let json = serde_json::to_value(&rsvp).unwrap();
        assert_eq!(rsvp, serde_json::from_value(json).unwrap());
    }
}
