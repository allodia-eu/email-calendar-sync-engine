//! The scheduling decision ([`reconcile`]) and the pure application of it to a
//! stored event ([`apply_reply`]/[`cancel`]).
//!
//! Reconciliation is the inbound half of the Write Contract: a trusted message
//! that supersedes the revision already applied for its [`InstanceKey`] yields a
//! [`ScheduleAction`] the host carries out (store the event, set a `PARTSTAT`,
//! cancel), while a stale, duplicate, untrusted, or staged-method message is
//! classified and surfaced, never silently applied (`calendar-semantics.md`).

use super::ScheduleMethod;
use super::key::{InstanceKey, Revision};
use super::message::SchedulingMessage;
use super::trust::{ImipTrust, ImipUntrusted, addresses_match};
use crate::calendar::{
    Event, EventStatus, Participant, ParticipationStatus, Recurrence, RecurrenceOverride,
};
use crate::time::{CalendarDateTime, LocalDateTime};

/// What an inbound scheduling message resolves to once trust and supersession are
/// decided.
///
/// The caller still holds the [`SchedulingMessage`], so the action stays lean:
/// [`ScheduleEvent`](ScheduleAction::ScheduleEvent)/[`Cancel`](ScheduleAction::Cancel)
/// operate on `message.event`/`message.instance_key()`, while
/// [`RecordReply`](ScheduleAction::RecordReply) distills the replying attendee and
/// status to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleAction {
    /// A `REQUEST` that supersedes: create the event if new, else update it in
    /// place (the carried event's attendees default to `needs-action`).
    ScheduleEvent,
    /// A `REPLY` that supersedes: set this attendee's `PARTSTAT` on the
    /// organizer's stored copy via [`apply_reply`].
    RecordReply {
        /// The replying attendee's calendar address.
        attendee: String,
        /// The status they conveyed.
        status: ParticipationStatus,
    },
    /// A `CANCEL` that supersedes: cancel the whole series or the targeted
    /// instance via [`cancel`].
    Cancel,
    /// A method whose full handling is staged (`PUBLISH`/`ADD`/`COUNTER`/
    /// `DECLINECOUNTER`/`REFRESH`, or an unknown/ill-formed method): classify and
    /// surface to the host; do not auto-apply (`calendar-semantics.md`).
    Surface(ScheduleMethod),
    /// The message does not supersede the revision already applied for its
    /// instance key: ignore it (a stale lower-`SEQUENCE` message, or an idempotent
    /// re-delivery).
    Superseded,
    /// The trust gate failed: the message must not be auto-applied.
    Rejected(ImipUntrusted),
}

/// Decides what to do with an inbound scheduling `message`.
///
/// `authenticated_sender` is the message's verified origin — a **bare calendar
/// address** (the extracted mailbox of From / DKIM / authenticated submission),
/// not a display-name/angle-bracket header; the caller normalizes it to that form,
/// since [`evaluate_imip_trust`](super::evaluate_imip_trust) compares it directly
/// against the body's `ORGANIZER`/`ATTENDEE`.
///
/// `current` is the highest [`Revision`] already applied for the relevant scope,
/// or `None` if unseen. The caller **must** scope it correctly: per
/// [`InstanceKey`] for organizer-originated methods (`REQUEST`/`CANCEL`), but
/// **per `(InstanceKey, attendee)` for a `REPLY`** — a `REPLY` carries the same
/// `SEQUENCE` as its `REQUEST`, so a single per-key revision would drop the first
/// RSVP as stale. The action itself carries no `SEQUENCE`, so this scoping is the
/// caller's responsibility.
///
/// The order is load-bearing: **trust first** (an untrusted message is rejected
/// before its contents are even considered), then **supersession** (a stale
/// message is dropped), then **method dispatch**.
#[must_use]
pub fn reconcile(
    message: &SchedulingMessage,
    authenticated_sender: Option<&str>,
    current: Option<&Revision>,
) -> ScheduleAction {
    if let ImipTrust::Untrusted(reason) = message.trust(authenticated_sender) {
        return ScheduleAction::Rejected(reason);
    }
    if let Some(current) = current
        && !message.revision().supersedes(current)
    {
        return ScheduleAction::Superseded;
    }
    match &message.method {
        ScheduleMethod::Request => ScheduleAction::ScheduleEvent,
        ScheduleMethod::Cancel => ScheduleAction::Cancel,
        ScheduleMethod::Reply => reply_action(message),
        other => ScheduleAction::Surface(other.clone()),
    }
}

/// Distills a trusted `REPLY` into a [`ScheduleAction::RecordReply`].
///
/// A `REPLY` that passed the trust gate always has a replying attendee with an
/// address (else trust would have failed with `MissingIdentity`); the `None` arm
/// is the defensive fallback that surfaces an ill-formed reply rather than
/// panicking.
fn reply_action(message: &SchedulingMessage) -> ScheduleAction {
    match message.replier() {
        Some(Participant {
            email: Some(attendee),
            participation_status,
            ..
        }) => ScheduleAction::RecordReply {
            attendee: attendee.clone(),
            status: participation_status.clone(),
        },
        _ => ScheduleAction::Surface(ScheduleMethod::Reply),
    }
}

/// Applies a trusted `REPLY` to the organizer's stored `event`: sets the matching
/// attendee's `participation_status` to `status`.
///
/// Returns `true` if a participant with `attendee`'s address was found and
/// updated. A reply from an address **not** on the event updates nothing and
/// returns `false` — the host surfaces it rather than inventing a participant
/// (`calendar-semantics.md` security: never apply changes from an unexpected
/// party).
pub fn apply_reply(event: &mut Event, attendee: &str, status: ParticipationStatus) -> bool {
    for participant in &mut event.participants {
        if participant
            .email
            .as_deref()
            .is_some_and(|email| addresses_match(email, attendee))
        {
            participant.participation_status = status;
            return true;
        }
    }
    false
}

/// Applies a trusted `CANCEL` to the stored `event`.
///
/// A **series** cancel (`target.recurrence_id` is `None`) sets the event's status
/// to `cancelled`, the model tombstone. An **instance** cancel excludes just the
/// targeted occurrence from the series (an `EXDATE`-like override on the master),
/// keyed by `exclusion_key` — so cancelling one all-day instance never tombstones
/// the whole series.
pub fn cancel(event: &mut Event, target: &InstanceKey) {
    match &target.recurrence_id {
        None => event.status = EventStatus::Cancelled,
        Some(recurrence_id) => {
            let key = exclusion_key(recurrence_id, &event.start);
            event
                .recurrence
                .get_or_insert_with(Recurrence::default)
                .overrides
                .insert(key, RecurrenceOverride::Excluded);
        }
    }
}

/// The override-map key for an instance-targeting `CANCEL`'s `recurrence_id`.
///
/// A timed value (floating/zoned) is its own wall clock. A `DATE` value resolves
/// against `series_start`'s time-of-day — midnight for an all-day series (so an
/// all-day instance keys at midnight), but the series' start time for a timed
/// series — so the exclusion matches the materialized occurrence. This mirrors the
/// iCalendar parser's `override_key` (`provider-caldav`), which keys all-day
/// `EXDATE`/`RECURRENCE-ID` overrides the same way.
fn exclusion_key(
    recurrence_id: &CalendarDateTime,
    series_start: &CalendarDateTime,
) -> LocalDateTime {
    match recurrence_id {
        CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => *local,
        CalendarDateTime::Date(date) => {
            let (hour, minute, second) = series_start
                .local()
                .map_or((0, 0, 0), |t| (t.hour(), t.minute(), t.second()));
            LocalDateTime::new(date.year(), date.month(), date.day(), hour, minute, second)
                .expect("a valid date and time-of-day form a valid local date-time")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{Event, Frequency, Participant, ParticipantRole, RecurrenceRule};
    use crate::ids::{CalendarId, EventId, Uid};
    use crate::membership::Memberships;
    use crate::time::{CalendarDateTime, UtcDateTime};

    fn floating(s: &str) -> CalendarDateTime {
        CalendarDateTime::Floating(s.parse().unwrap())
    }

    fn stamp(s: &str) -> UtcDateTime {
        s.parse().unwrap()
    }

    /// Builds a message with the given method, sequence, and dtstamp, organized by
    /// boss with attendee guest.
    fn message(method: ScheduleMethod, sequence: u32, dtstamp: &str) -> SchedulingMessage {
        let mut event = Event::new(
            EventId::try_from("imip:uid-1").unwrap(),
            Uid::new("uid-1").unwrap(),
            Memberships::of_one(CalendarId::try_from("imip:inbox").unwrap()),
            floating("2026-06-01T09:00:00"),
        );
        let mut organizer = Participant::attendee("boss@example.com");
        organizer.roles.insert(ParticipantRole::Owner);
        let guest = Participant::attendee("guest@example.com");
        event.participants = vec![organizer, guest];
        event.sequence = sequence;
        SchedulingMessage::new(method, event, stamp(dtstamp))
    }

    // --- Required test: trust gate (calendar-semantics.md) -------------------

    #[test]
    fn an_organizer_mismatch_is_not_auto_applied() {
        // A REQUEST whose ORGANIZER (boss) mismatches the authenticated sender
        // (an attacker) must be rejected, never applied.
        let request = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z");
        let action = reconcile(&request, Some("attacker@evil.example"), None);
        assert_eq!(
            action,
            ScheduleAction::Rejected(ImipUntrusted::SenderMismatch {
                expected: "organizer"
            })
        );
        // An unsigned (no authenticated sender) message is likewise rejected.
        assert_eq!(
            reconcile(&request, None, None),
            ScheduleAction::Rejected(ImipUntrusted::Unauthenticated)
        );
    }

    // --- Required test: REQUEST -> REPLY -> CANCEL by key/sequence -----------

    #[test]
    fn request_reply_cancel_reconcile_by_sequence_and_a_stale_request_loses() {
        let key = InstanceKey::series(Uid::new("uid-1").unwrap());

        // REQUEST (seq 0) from the organizer creates the event.
        let request = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z");
        assert_eq!(request.instance_key(), key);
        assert_eq!(
            reconcile(&request, Some("boss@example.com"), None),
            ScheduleAction::ScheduleEvent
        );
        let after_request = request.revision();

        // REPLY (seq 0, later dtstamp) from guest supersedes by DTSTAMP.
        let mut reply = message(ScheduleMethod::Reply, 0, "2026-05-01T09:00:00Z");
        reply.event.participants[1].participation_status = ParticipationStatus::Accepted;
        assert_eq!(
            reconcile(&reply, Some("guest@example.com"), Some(&after_request)),
            ScheduleAction::RecordReply {
                attendee: "guest@example.com".to_owned(),
                status: ParticipationStatus::Accepted,
            }
        );
        let after_reply = reply.revision();

        // CANCEL (seq 1) from the organizer supersedes by SEQUENCE.
        let cancel = message(ScheduleMethod::Cancel, 1, "2026-05-01T10:00:00Z");
        assert_eq!(
            reconcile(&cancel, Some("boss@example.com"), Some(&after_reply)),
            ScheduleAction::Cancel
        );
        let after_cancel = cancel.revision();

        // A stale, re-sent REQUEST (seq 0) does NOT override the newer CANCEL,
        // even though its DTSTAMP is later — SEQUENCE is compared first.
        let stale = message(ScheduleMethod::Request, 0, "2026-05-01T11:00:00Z");
        assert_eq!(
            reconcile(&stale, Some("boss@example.com"), Some(&after_cancel)),
            ScheduleAction::Superseded
        );
    }

    #[test]
    fn an_idempotent_redelivery_does_not_resupersede() {
        let request = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z");
        let current = request.revision();
        // The very same message arriving again (identical revision) is a no-op.
        assert_eq!(
            reconcile(&request, Some("boss@example.com"), Some(&current)),
            ScheduleAction::Superseded
        );
    }

    #[test]
    fn recurrence_id_targets_one_instance_key() {
        // A CANCEL carrying a RECURRENCE-ID reconciles against the instance key,
        // distinct from the series key — so cancelling one instance does not look
        // stale against the series and vice-versa.
        let mut cancel = message(ScheduleMethod::Cancel, 1, "2026-05-01T10:00:00Z");
        cancel.event.recurrence_id = Some(floating("2026-06-08T09:00:00"));
        let key = cancel.instance_key();
        assert!(!key.is_series());
        // Unseen instance key (None current) → applies.
        assert_eq!(
            reconcile(&cancel, Some("boss@example.com"), None),
            ScheduleAction::Cancel
        );
    }

    #[test]
    fn staged_methods_are_surfaced_not_applied() {
        for method in [
            ScheduleMethod::Counter,
            ScheduleMethod::Refresh,
            ScheduleMethod::Add,
            ScheduleMethod::Publish,
            ScheduleMethod::DeclineCounter,
        ] {
            let msg = message(method.clone(), 0, "2026-05-01T08:00:00Z");
            // COUNTER/DECLINECOUNTER need the right sender to pass trust; PUBLISH/
            // ADD/REFRESH are organizer/attendee-originated accordingly. Use the
            // matching identity so the dispatch (not trust) is what we observe.
            let sender = if method.is_organizer_originated() {
                "boss@example.com"
            } else {
                "guest@example.com"
            };
            assert_eq!(
                reconcile(&msg, Some(sender), None),
                ScheduleAction::Surface(method)
            );
        }
    }

    #[test]
    fn an_ill_formed_reply_with_no_attendee_address_is_surfaced() {
        // `reply_action`'s defensive fallback: reached only post-trust in
        // `reconcile` (where a replying address is guaranteed), it surfaces an
        // address-less reply rather than panicking when called directly.
        let mut msg = message(ScheduleMethod::Reply, 0, "2026-05-01T08:00:00Z");
        msg.event.participants[1].email = None;
        assert_eq!(
            reply_action(&msg),
            ScheduleAction::Surface(ScheduleMethod::Reply)
        );
    }

    // --- Apply helpers -------------------------------------------------------

    #[test]
    fn apply_reply_sets_the_matching_attendee_status() {
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        assert_eq!(
            event.participants[1].participation_status,
            ParticipationStatus::NeedsAction
        );
        // A scheme-/case-insensitive match still updates.
        assert!(apply_reply(
            &mut event,
            "MAILTO:Guest@example.com",
            ParticipationStatus::Declined
        ));
        assert_eq!(
            event.participants[1].participation_status,
            ParticipationStatus::Declined
        );
    }

    #[test]
    fn apply_reply_from_an_unknown_attendee_changes_nothing() {
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        let before = event.participants.clone();
        assert!(!apply_reply(
            &mut event,
            "stranger@example.com",
            ParticipationStatus::Accepted
        ));
        assert_eq!(event.participants, before);
    }

    #[test]
    fn series_cancel_tombstones_the_event() {
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        let uid = event.uid.clone();
        cancel(&mut event, &InstanceKey::series(uid));
        assert!(event.is_cancelled());
    }

    #[test]
    fn instance_cancel_excludes_just_that_occurrence() {
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
            Frequency::Weekly,
        )));
        let uid = event.uid.clone();
        let target = InstanceKey::instance(uid, floating("2026-06-08T09:00:00"));
        cancel(&mut event, &target);
        // The series is intact; only the one instance is excluded.
        assert!(!event.is_cancelled());
        assert!(
            event
                .recurrence
                .as_ref()
                .unwrap()
                .is_excluded(&"2026-06-08T09:00:00".parse().unwrap())
        );
    }

    #[test]
    fn instance_cancel_of_an_all_day_series_excludes_only_that_date() {
        use crate::time::CalendarDate;

        // An all-day recurring series; a CANCEL targeting ONE date must exclude only
        // that occurrence (keyed at midnight, the all-day override convention), never
        // tombstone the whole series.
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        event.start = CalendarDateTime::Date(CalendarDate::new(2026, 6, 1).unwrap());
        event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
        let uid = event.uid.clone();
        let target = InstanceKey::instance(
            uid,
            CalendarDateTime::Date(CalendarDate::new(2026, 6, 8).unwrap()),
        );
        cancel(&mut event, &target);
        assert!(
            !event.is_cancelled(),
            "the series must survive an instance cancel"
        );
        assert!(
            event
                .recurrence
                .as_ref()
                .unwrap()
                .is_excluded(&"2026-06-08T00:00:00".parse().unwrap())
        );
    }

    #[test]
    fn instance_cancel_of_a_date_recurrence_id_keys_at_the_series_time() {
        use crate::time::CalendarDate;

        // A timed series receiving a DATE-valued RECURRENCE-ID keys the exclusion at
        // the series' start time-of-day (09:00 here), matching how the parser keys a
        // DATE override against a timed series (`override_key`).
        let mut event = message(ScheduleMethod::Request, 0, "2026-05-01T08:00:00Z").event;
        // `start` is the floating 2026-06-01T09:00:00 from `message`.
        event.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(Frequency::Daily)));
        let uid = event.uid.clone();
        let target = InstanceKey::instance(
            uid,
            CalendarDateTime::Date(CalendarDate::new(2026, 6, 8).unwrap()),
        );
        cancel(&mut event, &target);
        assert!(
            event
                .recurrence
                .as_ref()
                .unwrap()
                .is_excluded(&"2026-06-08T09:00:00".parse().unwrap())
        );
    }
}
