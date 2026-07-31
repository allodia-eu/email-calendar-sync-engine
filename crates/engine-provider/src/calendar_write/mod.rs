//! Outbound calendar write shapes.
//!
//! These mirror the mail [`Draft`](crate::Draft)/[`SubmissionReceipt`](crate::SubmissionReceipt)
//! pair: serializable requests a caller stores as a durable outbox `PendingOp` payload
//! before the side effect, plus a receipt the outbox records on success.
//!
//! # The four neutral verbs, and the one that is not
//!
//! [`EventDraft`] (create), [`EventEdit`] (patch), [`EventDeletion`] (delete) and
//! [`EventRsvp`] (answer an invitation) are the spine, and every calendar adapter implements
//! all four. They carry **intent** — a title, a new start, which occurrence, yes or no — and
//! each adapter renders that intent in its own protocol. So a host never touches a
//! `RawIcal`, an href or an `ETag` to edit an event, and never switches on provider kind
//! (`providers.md`).
//!
//! [`EventRsvp`] is a verb of its own rather than an [`EventEdit`] of the attendee array for
//! a reason the bytes hide: answering an invitation makes the server **tell the organizer**,
//! and a patch does not. Graph, Google and every auto-scheduling CalDAV/JMAP server route it
//! through a distinct path; expressing it as an edit would change the same participant and
//! skip the scheduling silently.
//!
//! [`EventWrite`] is the exception, and is deliberately *not* part of that spine: it
//! replaces the whole stored document, which only a **document-oriented** transport has as
//! a verb (CalDAV `PUT`, RFC 4791 §5.3.2 — the client owns the bytes). A transport whose
//! update verb is already a patch (JMAP `CalendarEvent/set`) has no such thing and leaves
//! it unsupported. It exists because some operations are naturally expressed as a finished
//! document rather than a property patch — today, the iMIP RSVP primitive
//! (`provider_caldav::imip::set_my_partstat`), which rewrites *my* `PARTSTAT` inside the
//! stored iCalendar and hands back the bytes to store.
//!
//! # Never re-serialize the projection
//!
//! The engine's [`Event`] projection is deliberately lossy (`calendar-semantics.md`): it has
//! no room for the `RRULE`'s `BYSETPOS`, the attendees' `DELEGATED-FROM`, the `VALARM`s, the
//! embedded `VTIMEZONE`, the `X-` properties another client wrote. Rebuilding a stored event
//! from it to move the event by half an hour would silently delete every one of them — a
//! save that looks like it worked. So an **update is always a patch**, applied to the
//! provider-native payload as it was received: the adapter does the surgery over the stored
//! raw (CalDAV), or hands the patch to a server that does (JMAP). A create is the one place
//! a document is built from scratch, because there is nothing yet to lose.
//!
//! # The lost-update guard
//!
//! Every write names the revision the caller read — a patch and a delete through the
//! [`Event`] they were built from, a document write through its own guard — so a server can
//! refuse an edit built on a copy that has since moved on. **Whether it does is not
//! universal**: read
//! [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard) before
//! writing. Under [`WriteGuard::Absent`](crate::WriteGuard::Absent) a stale write silently
//! wins, so "the write succeeded" does not mean "no concurrent edit was lost".

mod patch;
mod rsvp;

use engine_core::{
    calendar::Event,
    ids::{CalendarId, EventId, Uid},
    raw::RawIcal,
    time::{CalendarDateTime, UtcDateTime},
    version::RevisionTokens,
};
pub use patch::{EventEdit, EventPatch, PatchTarget, TextEdit};
pub use rsvp::{EventRsvp, RsvpResponse};
use serde::{Deserialize, Serialize};

/// A new event to create.
///
/// Carries intent, not a document: the adapter serializes it. CalDAV builds an iCalendar
/// object and `PUT`s it under `If-None-Match: *`; JMAP posts a JSCalendar object to
/// `CalendarEvent/set` `create`, and the **server** assigns the id — so the resulting
/// [`EventId`] is learned from the [`EventWriteReceipt`], never minted by the caller.
///
/// The [`Uid`] *is* the caller's to mint: it is the cross-system event identity, and it is
/// what lets a retried create be recognized as the same event on either transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDraft {
    /// The calendar the event lands in.
    pub calendar: CalendarId,
    /// The cross-system `UID`, minted by the caller.
    pub uid: Uid,
    /// The title.
    pub summary: String,
    /// The start.
    pub start: CalendarDateTime,
    /// The end. For an all-day event this is **exclusive** (RFC 5545 §3.6.1): a one-day
    /// event on the 1st ends on the 2nd.
    pub end: CalendarDateTime,
    /// The description, if any.
    pub description: Option<String>,
    /// The location, if any. A create is the one write that can set it from nothing;
    /// an edit already reshapes it through [`EventPatch::location`](crate::EventPatch::location).
    pub location: Option<String>,
    /// When the event was created — the caller's, because engine time types deliberately
    /// cannot read the system clock. A server that stamps its own ignores it.
    pub stamp: UtcDateTime,
}

impl EventDraft {
    /// A new event in `calendar`, running from `start` to `end`.
    #[must_use]
    pub fn new(
        calendar: CalendarId,
        uid: Uid,
        summary: impl Into<String>,
        start: CalendarDateTime,
        end: CalendarDateTime,
        stamp: UtcDateTime,
    ) -> Self {
        Self {
            calendar,
            uid,
            summary: summary.into(),
            start,
            end,
            description: None,
            location: None,
            stamp,
        }
    }

    /// Gives the new event a description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Gives the new event a location.
    #[must_use]
    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(location.into());
        self
    }
}

/// A request to replace the whole stored calendar document.
///
/// The write verb of a **document-oriented** transport only — see the module docs. The
/// `ical` is the provider-native payload the caller assembled (round-tripped from the
/// stored raw plus targeted edits, *never* re-serialized from the projection); an adapter
/// with no document verb rejects this as unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWrite {
    /// The event whose document is being replaced.
    pub event: EventId,
    /// Its cross-system `UID`, echoed on the receipt for reconciliation.
    pub uid: Uid,
    /// The document to store.
    pub ical: RawIcal,
    /// The revision the write is guarded by — the one the caller read. `None` replaces
    /// unconditionally.
    pub guard: Option<RevisionTokens>,
}

impl EventWrite {
    /// Replaces the document of `base` — the event as the caller read it — guarded by the
    /// revision it was read at.
    #[must_use]
    pub fn replacing(base: &Event, ical: RawIcal) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            ical,
            guard: Some(base.revisions.clone()),
        }
    }

    /// Replaces the document with **no** guard, so it lands over whatever the server holds.
    #[must_use]
    pub fn unconditional(event: EventId, uid: Uid, ical: RawIcal) -> Self {
        Self {
            event,
            uid,
            ical,
            guard: None,
        }
    }
}

/// A request to delete an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDeletion {
    /// The event to delete.
    pub event: EventId,
    /// Its cross-system `UID`. Carried here rather than passed alongside because the outbox
    /// serializes every calendar op on it, and a caller that supplied it separately could
    /// supply the *wrong* one — pairing a delete of event A with the lock on event B.
    pub uid: Uid,
    /// The revision the delete is guarded by — the one the caller read. `None` deletes
    /// unconditionally.
    pub guard: Option<RevisionTokens>,
}

impl EventDeletion {
    /// Deletes `base` — the event as the caller read it — guarded by the revision it was
    /// read at, so the delete cannot silently discard someone else's newer edit.
    #[must_use]
    pub fn of(base: &Event) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            guard: Some(base.revisions.clone()),
        }
    }

    /// Deletes with **no** guard: the event goes, whatever the server holds.
    #[must_use]
    pub fn unconditional(event: EventId, uid: Uid) -> Self {
        Self {
            event,
            uid,
            guard: None,
        }
    }
}

/// The result of a successful calendar write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWriteReceipt {
    /// The event now backing the object. For a **create** this is the id the write resolved
    /// to — which a server-assigning transport (JMAP) reveals only here.
    pub event: EventId,
    /// The event's `UID`, echoed for sync-time reconciliation.
    pub uid: Uid,
    /// The revision tokens the write's response carried, if any. CalDAV supplies the new
    /// `ETag` when the server returns one on the `PUT` (RFC 4791 §5.3.4 recommends it);
    /// JMAP supplies none, because a `CalendarEvent` has no per-object revision. An empty
    /// set means the caller learns the new revision from the next sync.
    pub revisions: RevisionTokens,
}

impl EventWriteReceipt {
    /// Records a successful write.
    #[must_use]
    pub fn new(event: EventId, uid: Uid, revisions: RevisionTokens) -> Self {
        Self {
            event,
            uid,
            revisions,
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(write.guard.unwrap().etag, Some(ETag::new("\"v7\"")));
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

        let write = EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"));
        let encoded = serde_json::to_value(&write).unwrap();
        assert_eq!(
            serde_json::from_value::<EventWrite>(encoded).unwrap(),
            write
        );

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
}
