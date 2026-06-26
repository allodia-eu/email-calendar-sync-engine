//! The normalized, parsed inbound iTIP message ([`SchedulingMessage`]).
//!
//! An iTIP message is a `METHOD` plus a calendar object (RFC 5546 §3): one
//! `VEVENT` (the series master or a single `RECURRENCE-ID` instance) and a
//! `DTSTAMP`. This crate owns the *shape* — the [`Event`] projection plus the
//! method and message timestamp — from which the reconciliation key, revision,
//! and trust identities are derived. Producing it from a `text/calendar` body is
//! the iCalendar parser's job (`provider-caldav`), so the same normalized event
//! the calendar sync produces also backs scheduling.

use serde::{Deserialize, Serialize};

use super::ScheduleMethod;
use super::key::{InstanceKey, Revision};
use super::trust::{ImipTrust, evaluate_imip_trust};
use crate::calendar::{Event, Participant, ParticipantRole, ParticipationStatus};
use crate::time::UtcDateTime;

/// A parsed inbound iTIP scheduling message.
///
/// The carried [`Event`] is the normalized projection of the message's `VEVENT`;
/// its `uid`, `recurrence_id`, `sequence`, `participants`, and `status` drive
/// reconciliation, while its `raw_ical` preserves the wire form for an RSVP that
/// round-trips from raw. The `EventId`/`CalendarId` on a parsed message are
/// **synthetic placeholders** (an iMIP body has no provider href/collection);
/// the real storage identity is assigned when the event is stored. Reconciliation
/// keys on [`InstanceKey`]/[`Revision`], never on those ids (`calendar-semantics.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulingMessage {
    /// The iTIP method (`METHOD`).
    pub method: ScheduleMethod,
    /// The carried calendar object, normalized.
    pub event: Event,
    /// The message timestamp (`DTSTAMP`), the [`Revision`] tie-breaker.
    pub dtstamp: UtcDateTime,
}

impl SchedulingMessage {
    /// Creates a scheduling message.
    #[must_use]
    pub fn new(method: ScheduleMethod, event: Event, dtstamp: UtcDateTime) -> Self {
        Self {
            method,
            event,
            dtstamp,
        }
    }

    /// The reconciliation target: the event's `(UID, RECURRENCE-ID)`.
    #[must_use]
    pub fn instance_key(&self) -> InstanceKey {
        InstanceKey {
            uid: self.event.uid.clone(),
            recurrence_id: self.event.recurrence_id.clone(),
        }
    }

    /// The message revision: the event's `SEQUENCE` and this message's `DTSTAMP`.
    #[must_use]
    pub fn revision(&self) -> Revision {
        Revision::new(self.event.sequence, self.dtstamp)
    }

    /// The `ORGANIZER`'s calendar address (the participant carrying the `owner`
    /// role), if present.
    #[must_use]
    pub fn organizer(&self) -> Option<&str> {
        self.event
            .participants
            .iter()
            .find(|p| p.has_role(&ParticipantRole::Owner))
            .and_then(|p| p.email.as_deref())
    }

    /// The replying attendee of a `REPLY`: the party whose `PARTSTAT` this message
    /// conveys.
    ///
    /// Normally the first non-organizer participant (a `REPLY` carries the organizer
    /// plus the one replying attendee, RFC 5546 §3.2.3). For a **self-organized**
    /// event the iCalendar parser merges an `ORGANIZER` who is also an `ATTENDEE`
    /// into one participant carrying both roles; when that is the only participant it
    /// *is* the replier, so the fallback returns it rather than dropping a valid
    /// self-RSVP.
    #[must_use]
    pub fn replier(&self) -> Option<&Participant> {
        self.event
            .participants
            .iter()
            .find(|p| !p.has_role(&ParticipantRole::Owner))
            .or_else(|| self.event.participants.first())
    }

    /// The replying attendee's calendar address, if present.
    #[must_use]
    pub fn replying_attendee(&self) -> Option<&str> {
        self.replier().and_then(|p| p.email.as_deref())
    }

    /// The participation status the replying attendee conveys, if any.
    #[must_use]
    pub fn reply_status(&self) -> Option<&ParticipationStatus> {
        self.replier().map(|p| &p.participation_status)
    }

    /// Evaluates whether this message may be auto-applied: its authenticated
    /// sender must match the body identity it acts as (the `ORGANIZER` for an
    /// organizer-originated method, the replying `ATTENDEE` otherwise).
    #[must_use]
    pub fn trust(&self, authenticated_sender: Option<&str>) -> ImipTrust {
        evaluate_imip_trust(
            &self.method,
            self.organizer(),
            self.replying_attendee(),
            authenticated_sender,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::ParticipationStatus;
    use crate::ids::{CalendarId, EventId, Uid};
    use crate::membership::Memberships;
    use crate::time::{CalendarDateTime, LocalDateTime};

    fn at(s: &str) -> CalendarDateTime {
        CalendarDateTime::Floating(s.parse().unwrap())
    }

    /// A REQUEST from organizer boss with attendee guest (needs-action).
    fn request() -> SchedulingMessage {
        let mut event = Event::new(
            EventId::try_from("imip:uid-1").unwrap(),
            Uid::new("uid-1").unwrap(),
            Memberships::of_one(CalendarId::try_from("imip:inbox").unwrap()),
            at("2026-06-01T09:00:00"),
        );
        let mut organizer = Participant::attendee("boss@example.com");
        organizer.roles.insert(ParticipantRole::Owner);
        let guest = Participant::attendee("guest@example.com");
        event.participants = vec![organizer, guest];
        event.sequence = 0;
        SchedulingMessage::new(
            ScheduleMethod::Request,
            event,
            "2026-05-01T08:00:00Z".parse().unwrap(),
        )
    }

    #[test]
    fn derives_key_and_revision() {
        let msg = request();
        assert_eq!(
            msg.instance_key(),
            InstanceKey::series(Uid::new("uid-1").unwrap())
        );
        assert!(msg.instance_key().is_series());
        assert_eq!(
            msg.revision(),
            Revision::new(0, "2026-05-01T08:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn instance_key_carries_recurrence_id() {
        let mut msg = request();
        msg.event.recurrence_id = Some(at("2026-06-08T09:00:00"));
        let key = msg.instance_key();
        assert!(!key.is_series());
        assert_eq!(key.recurrence_id, Some(at("2026-06-08T09:00:00")));
    }

    #[test]
    fn organizer_and_replier_resolve_by_role() {
        let msg = request();
        assert_eq!(msg.organizer(), Some("boss@example.com"));
        // The replier is the first non-owner participant (the attendee).
        assert_eq!(msg.replying_attendee(), Some("guest@example.com"));
    }

    #[test]
    fn a_self_organized_replier_is_the_owner_attendee() {
        // A self-organized event the parser merges into ONE participant carrying both
        // Owner and Attendee roles (organizer == attendee). That participant is the
        // replier, so a self-RSVP resolves and is trusted rather than dropped.
        let mut msg = request();
        msg.method = ScheduleMethod::Reply;
        let mut boss = Participant::attendee("boss@example.com");
        boss.roles.insert(ParticipantRole::Owner);
        boss.participation_status = ParticipationStatus::Accepted;
        msg.event.participants = vec![boss];
        assert_eq!(msg.replying_attendee(), Some("boss@example.com"));
        assert_eq!(msg.reply_status(), Some(&ParticipationStatus::Accepted));
        assert_eq!(msg.trust(Some("boss@example.com")), ImipTrust::Trusted);
    }

    #[test]
    fn trust_checks_organizer_for_a_request() {
        let msg = request();
        assert_eq!(msg.trust(Some("boss@example.com")), ImipTrust::Trusted);
        // A REQUEST authenticating as the attendee (not the organizer) is untrusted.
        assert!(matches!(
            msg.trust(Some("guest@example.com")),
            ImipTrust::Untrusted(_)
        ));
    }

    #[test]
    fn reply_status_reads_the_attendee() {
        let mut msg = request();
        msg.method = ScheduleMethod::Reply;
        // guest accepts.
        msg.event.participants[1].participation_status = ParticipationStatus::Accepted;
        assert_eq!(msg.reply_status(), Some(&ParticipationStatus::Accepted));
        assert_eq!(msg.trust(Some("guest@example.com")), ImipTrust::Trusted);
    }

    #[test]
    fn roundtrips_through_json() {
        let msg = request();
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(
            serde_json::from_str::<SchedulingMessage>(&json).unwrap(),
            msg
        );
        // Sanity: a zoned recurrence id survives too.
        let mut instance = msg;
        instance.event.recurrence_id = Some(CalendarDateTime::Zoned {
            local: LocalDateTime::new(2026, 6, 8, 9, 0, 0).unwrap(),
            zone: crate::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        });
        let json = serde_json::to_string(&instance).unwrap();
        assert_eq!(
            serde_json::from_str::<SchedulingMessage>(&json).unwrap(),
            instance
        );
    }
}
