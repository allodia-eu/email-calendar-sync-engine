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
    calendar::{Event, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    raw::RawIcal,
    time::{CalendarDateTime, UtcDateTime},
    version::RevisionTokens,
};
pub use patch::{EventEdit, EventPatch, PatchTarget, RecurrenceEdit, TextEdit};
pub use rsvp::{EventRsvp, ReplyDelivery, RsvpResponse};
use serde::{Deserialize, Serialize};

/// How a new event repeats.
///
/// A rule, plus the one thing about it no adapter can work out for itself.
///
/// # Why the instant rides along
///
/// [`RecurrenceBound::Until`](engine_core::calendar::RecurrenceBound::Until) holds a **wall
/// clock in the event's own zone** — the JSCalendar reading, which is what the engine model
/// follows. iCalendar disagrees: RFC 5545 §3.3.10 requires `UNTIL` **in UTC** whenever
/// `DTSTART` is zoned or UTC, and resolving a wall clock through a zone needs tzdata, which
/// lives in `engine-recurrence` and which no adapter (nor `engine-core`, nor `engine-ical`)
/// depends on. So the caller resolves it once — `engine_api::resolve_instant` is the
/// resolver — and the answer travels with the draft.
///
/// It rides *here*, on the durable payload, rather than being passed beside it, because the
/// outbox serializes an [`EventDraft`] before the side effect: a create replayed after a
/// restart must not have to redo zone maths whose tz database may since have moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftRecurrence {
    /// The rule the series follows.
    pub rule: RecurrenceRule,
    /// The rule's `UNTIL` resolved to an instant through the event's own zone.
    ///
    /// Required exactly when the rule ends at a wall clock **and** the draft's
    /// [`start`](EventDraft::start) is zoned or UTC; an adapter that needs it and does not
    /// have it refuses the write rather than emitting a local `UNTIL` an RFC 5545 reader
    /// would misread.
    ///
    /// `None` otherwise: an unbounded or counted rule has no `UNTIL` at all, and a floating
    /// or all-day series renders it from the rule's own wall clock, which needs no zone.
    pub until: Option<UtcDateTime>,
}

impl DraftRecurrence {
    /// A series following `rule`, with no `UNTIL` to resolve — unbounded, counted, or on a
    /// floating/all-day event.
    #[must_use]
    pub fn new(rule: RecurrenceRule) -> Self {
        Self { rule, until: None }
    }

    /// A series following `rule`, whose `UNTIL` resolves to `until` in the event's own zone.
    #[must_use]
    pub fn ending_at(rule: RecurrenceRule, until: UtcDateTime) -> Self {
        Self {
            rule,
            until: Some(until),
        }
    }
}

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
    /// How the event repeats, or `None` for a one-off.
    ///
    /// Changing or removing the rule afterwards goes through
    /// [`EventPatch::recurrence`](crate::EventPatch::recurrence).
    pub recurrence: Option<DraftRecurrence>,
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
            recurrence: None,
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

    /// Makes the new event repeat.
    ///
    /// See [`DraftRecurrence`] for why a rule ending at a wall clock carries a resolved
    /// instant beside it.
    #[must_use]
    pub fn repeating(mut self, recurrence: DraftRecurrence) -> Self {
        self.recurrence = Some(recurrence);
        self
    }
}

/// What a document write asks the server to verify **before** it stores anything.
///
/// Three states rather than "a guard or no guard", because storing a document is not
/// always a replace. Putting an invitation that arrived as mail onto the calendar is a
/// **create**, and a create's precondition is the opposite of an update's: not "the
/// revision I read is still current" but "nothing is there at all". Collapsing the two
/// leaves a guarded create unrepresentable, and an unconditional write is what a caller
/// then falls back to — which silently overwrites a resource that appeared in the
/// meantime, exactly the case a create is most likely to hit (the server scheduled the
/// meeting a moment ago, or a second device stored it first).
///
/// Named for the condition, not for the HTTP headers that happen to express it on
/// CalDAV: a transport is free to render these however it can, and one that cannot
/// render a state at all says so through
/// [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WritePrecondition {
    /// Nothing is checked: the document lands over whatever the server holds.
    Unconditional,
    /// The resource must still be at the revision the caller read, so a stale edit cannot
    /// overwrite a newer one. CalDAV renders this `If-Match: <etag>`.
    ///
    /// An **empty** [`RevisionTokens`] still means "I asked for a guard" — it is a caller
    /// who read an object whose transport carries no revision, which is not the same as a
    /// caller who waived one. What that guard is *worth* is
    /// [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard).
    IfUnchanged(RevisionTokens),
    /// Nothing may exist at the target yet — a **create**. A resource already there is a
    /// [`Conflict`](engine_core::error::FailureClass::Conflict), never a silent overwrite.
    /// CalDAV renders this `If-None-Match: *` (RFC 7232 §3.2).
    IfAbsent,
}

/// A request to store a whole calendar document — replacing what is there, or creating
/// where nothing is.
///
/// The write verb of a **document-oriented** transport only — see the module docs. The
/// `ical` is the provider-native payload the caller assembled (round-tripped from the
/// stored raw plus targeted edits, *never* re-serialized from the projection); an adapter
/// with no document verb rejects this as unsupported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventWrite {
    /// The event whose document is being stored.
    pub event: EventId,
    /// Its cross-system `UID`, echoed on the receipt for reconciliation.
    pub uid: Uid,
    /// The document to store.
    pub ical: RawIcal,
    /// What the server must verify before storing it.
    pub guard: WritePrecondition,
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
            guard: WritePrecondition::IfUnchanged(base.revisions.clone()),
        }
    }

    /// Stores the document only if **nothing is there yet**.
    ///
    /// The guarded create: a resource already at `event` is a
    /// [`Conflict`](engine_core::error::FailureClass::Conflict) the caller resolves by
    /// re-reading, never an overwrite it never learns about. This is how an invitation that
    /// arrived as mail is put on the calendar with its `ORGANIZER`, `ATTENDEE`, `UID` and
    /// `SEQUENCE` intact — [`EventDraft`] carries none of those, so a create through the
    /// neutral spine would store a plain appointment with no `ATTENDEE` line to answer on
    /// afterwards.
    #[must_use]
    pub fn creating(event: EventId, uid: Uid, ical: RawIcal) -> Self {
        Self {
            event,
            uid,
            ical,
            guard: WritePrecondition::IfAbsent,
        }
    }

    /// Stores the document with **no** precondition, so it lands over whatever the server
    /// holds.
    #[must_use]
    pub fn unconditional(event: EventId, uid: Uid, ical: RawIcal) -> Self {
        Self {
            event,
            uid,
            ical,
            guard: WritePrecondition::Unconditional,
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
    /// For an **RSVP**, what the server said about getting the answer to the organizer.
    ///
    /// [`NotReported`](ReplyDelivery::NotReported) on every other verb, and on any transport
    /// that does not report — which is most of them. Read [`ReplyDelivery`] before acting on
    /// it; in particular, silence is not success.
    pub reply_delivery: ReplyDelivery,
}

impl EventWriteReceipt {
    /// Records a successful write, with nothing reported about scheduling.
    ///
    /// Every verb but an RSVP wants this, and so does an RSVP on a transport that reports
    /// nothing — which is most of them. That is why a reporting adapter opts *in* via
    /// [`with_reply_delivery`](Self::with_reply_delivery) rather than every other caller
    /// opting out: an adapter that has never heard of this says "I don't know", which is
    /// true, instead of claiming a delivery it never observed.
    #[must_use]
    pub fn new(event: EventId, uid: Uid, revisions: RevisionTokens) -> Self {
        Self {
            event,
            uid,
            revisions,
            reply_delivery: ReplyDelivery::NotReported,
        }
    }

    /// Records what the server reported about delivering an RSVP to the organizer.
    #[must_use]
    pub fn with_reply_delivery(mut self, delivery: ReplyDelivery) -> Self {
        self.reply_delivery = delivery;
        self
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
