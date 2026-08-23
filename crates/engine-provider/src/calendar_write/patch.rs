//! The **intent** of an event edit: which occurrence it lands on, and what it changes.
//!
//! These types say *what the user did*, never *how a protocol expresses it*. That split is
//! the whole point: the surgery differs completely per transport, but the intent does not.
//!
//! - **CalDAV** has no partial write. `PUT` replaces the whole resource, so the *client* must do
//!   the surgery — hence `provider_caldav`'s structural iCalendar patcher, which rewrites only the
//!   touched content lines of the stored `RawIcal` and leaves the `VALARM`s, the `VTIMEZONE`, the
//!   `X-` properties and the original folding byte-for-byte intact.
//! - **JMAP** `CalendarEvent/set` `update` already *is* a patch (an RFC 8620 §5.3 PatchObject keyed
//!   by JSON pointer), so the **server** does the surgery and the adapter only has to translate the
//!   intent into pointers.
//!
//! Hoisting the patcher itself would therefore have meant hoisting RFC 5545 line folding,
//! `DTEND`-vs-`DURATION` exclusion and `SEQUENCE` bookkeeping into a crate whose other
//! implementer has no use for any of it. Hoisting the *intent* costs nothing and is what
//! makes a host provider-agnostic.
//!
//! # Two rules that really are universal
//!
//! Both are silent-corruption bugs if an adapter gets them wrong, and both bite on every
//! calendar protocol — which is the evidence that they belong here rather than in one
//! adapter:
//!
//! - **A move must never resolve a zoned event to a UTC instant.** iCalendar states it as
//!   `DTSTART;TZID=…`, JSCalendar as `start` + `timeZone`; either way, writing back the instant
//!   instead of the wall clock moves the event for every reader in another zone, and re-times the
//!   whole series when the zone next crosses a DST boundary. So a [`start`](EventPatch::start) must
//!   keep the event's existing *form* — an adapter rejects a form change rather than converting it.
//! - **Series or one occurrence is a question for the user, never a default.** Hence
//!   [`PatchTarget`] has no `Default`: a drag on Tuesday's standup is either a move of *that
//!   occurrence* or of *every Tuesday from now to eternity*, and only the user knows which.

use engine_core::{
    calendar::Event,
    ids::{EventId, Uid},
    time::{CalendarDateTime, UtcDateTime},
};
use serde::{Deserialize, Serialize};

use super::{DraftRecurrence, Occurrence};

/// Which occurrence of a recurring event an [`EventPatch`] lands on.
///
/// There is no `Default` — see the module docs. For a non-recurring event only
/// [`PatchTarget::Series`] is meaningful (it is the event's only instance).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatchTarget {
    /// The series itself. **Every** occurrence moves.
    ///
    /// iCalendar: the master `VEVENT` (the one with no `RECURRENCE-ID`). JSCalendar: the
    /// top-level object, leaving `recurrenceOverrides` alone.
    Series,
    /// A single occurrence — see [`Occurrence`] for how one is named, and why naming it
    /// takes more than a wall clock.
    ///
    /// iCalendar: the `RECURRENCE-ID` override `VEVENT` (RFC 5545 §3.8.4.4). JSCalendar:
    /// the `recurrenceOverrides` entry keyed by that start (RFC 8984 §4.3.3). Graph and
    /// Google patch the occurrence at an id they derive from it, which is why the resolved
    /// instant matters here as much as it does on a delete.
    ///
    /// **Creating** an override the series does not have yet may need more than the patch
    /// itself carries: an adapter that must split the occurrence out of the series by hand
    /// (CalDAV does; the copy would otherwise inherit the *first* occurrence's times)
    /// requires this occurrence's [`start`](EventPatch::start) **and** [`end`](EventPatch::end)
    /// on the patch — pass both, unchanged, even when the edit does not move the event. An
    /// adapter whose server materializes the override (JMAP) needs neither.
    Instance(Occurrence),
}

/// What an edit does to an optional text property: give it a value, or take it away.
///
/// Distinct from *not mentioning it*, which leaves the property exactly as it was — three
/// states, so `Option<TextEdit>` rather than a nested `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextEdit {
    /// Write this value, replacing any current one.
    Set(String),
    /// Remove the property.
    Clear,
}

/// What an edit does to the event's recurrence: give it a rule, or take its rule away.
///
/// Three states with `Option<RecurrenceEdit>`, on the same reasoning as [`TextEdit`]: *not
/// mentioning* recurrence leaves the series exactly as it was, which is not the same as
/// turning a repeating event into a single one.
///
/// Only a [`Series`](PatchTarget::Series) target may carry one. A single occurrence has no
/// rule of its own — it is one instance *of* a rule — so an adapter refuses the pairing
/// rather than writing something whose meaning it would have to invent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecurrenceEdit {
    /// Replace the rule, or give a one-off event its first one.
    ///
    /// Carries a [`DraftRecurrence`] rather than a bare rule for the reason a create does:
    /// an `UNTIL` on a zoned event has to reach iCalendar and Google in UTC, and no adapter
    /// carries the tzdata to work that out.
    ///
    /// Boxed because a rule holds ten `by*` vectors and this enum's other variant holds
    /// nothing: unboxed, every [`EventPatch`] — including the overwhelming majority that
    /// touch no recurrence at all — would carry that size through the outbox and every
    /// clone. The `Box` is invisible at a match site, which derefs.
    Set(Box<DraftRecurrence>),
    /// Remove the recurrence: every occurrence but the first goes, and the event becomes a
    /// single one.
    Clear,
}

/// The properties an edit changes. Anything not set here is left exactly as it was.
///
/// Built with a consuming `with`-style chain from [`EventPatch::new`], so an unset property
/// and a cleared one can never be confused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPatch {
    stamp: UtcDateTime,
    summary: Option<String>,
    description: Option<TextEdit>,
    location: Option<TextEdit>,
    start: Option<CalendarDateTime>,
    end: Option<CalendarDateTime>,
    recurrence: Option<RecurrenceEdit>,
}

impl EventPatch {
    /// An empty patch, stamped with the instant the user made the edit.
    ///
    /// Applying it changes nothing but that stamp — every other byte of the stored event
    /// survives.
    ///
    /// The stamp is the caller's because engine time types deliberately cannot read the
    /// system clock. What an adapter does with it differs, and neither is a lie: a
    /// **client-stamped** transport writes it (CalDAV: `DTSTAMP`, and `LAST-MODIFIED` where
    /// the event already has one — the iTIP revision bookkeeping is the client's job there),
    /// while a **server-stamped** one ignores it and sets its own (JMAP: the server owns
    /// JSCalendar `updated`).
    #[must_use]
    pub fn new(stamp: UtcDateTime) -> Self {
        Self {
            stamp,
            summary: None,
            description: None,
            location: None,
            start: None,
            end: None,
            recurrence: None,
        }
    }

    /// Sets the title.
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Sets the description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(TextEdit::Set(description.into()));
        self
    }

    /// Removes the description — distinct from not mentioning it, which keeps it.
    #[must_use]
    pub fn clear_description(mut self) -> Self {
        self.description = Some(TextEdit::Clear);
        self
    }

    /// Sets the location.
    #[must_use]
    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(TextEdit::Set(location.into()));
        self
    }

    /// Removes the location — distinct from not mentioning it, which keeps it.
    #[must_use]
    pub fn clear_location(mut self) -> Self {
        self.location = Some(TextEdit::Clear);
        self
    }

    /// Moves the event's start.
    ///
    /// The value must keep the event's existing **form** — a zoned event stays in its own
    /// zone, an all-day event stays a date — or the adapter errors rather than converting
    /// it (see the module docs). Shift the wall clock of the event's current start; do not
    /// resolve it to an instant first.
    #[must_use]
    pub fn start(mut self, start: CalendarDateTime) -> Self {
        self.start = Some(start);
        self
    }

    /// Moves the event's end, under the same form rule as [`start`](Self::start).
    ///
    /// For an **all-day** event the end is *exclusive* (RFC 5545 §3.6.1): a one-day event
    /// on the 1st ends on the 2nd. Passing the last day instead silently shortens the event
    /// by a day. An adapter whose model states a duration rather than an end (JSCalendar
    /// `duration`) derives it from the start.
    #[must_use]
    pub fn end(mut self, end: CalendarDateTime) -> Self {
        self.end = Some(end);
        self
    }

    /// Replaces the event's recurrence — or gives a one-off event its first rule.
    ///
    /// Valid only with a [`Series`](PatchTarget::Series) target; see [`RecurrenceEdit`].
    #[must_use]
    pub fn recurrence(mut self, recurrence: DraftRecurrence) -> Self {
        self.recurrence = Some(RecurrenceEdit::Set(Box::new(recurrence)));
        self
    }

    /// Turns a repeating event into a single one — distinct from not mentioning recurrence,
    /// which leaves the series alone.
    #[must_use]
    pub fn clear_recurrence(mut self) -> Self {
        self.recurrence = Some(RecurrenceEdit::Clear);
        self
    }

    /// What the edit does to the recurrence, if it touches it.
    #[must_use]
    pub fn recurrence_edit(&self) -> Option<&RecurrenceEdit> {
        self.recurrence.as_ref()
    }

    /// When the user made this edit. See [`new`](Self::new) for who honours it.
    #[must_use]
    pub fn stamp(&self) -> UtcDateTime {
        self.stamp
    }

    /// The new title, if the edit sets one.
    #[must_use]
    pub fn summary_edit(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// What the edit does to the description, if it touches it.
    #[must_use]
    pub fn description_edit(&self) -> Option<&TextEdit> {
        self.description.as_ref()
    }

    /// What the edit does to the location, if it touches it.
    #[must_use]
    pub fn location_edit(&self) -> Option<&TextEdit> {
        self.location.as_ref()
    }

    /// The new start, if the edit moves the event.
    #[must_use]
    pub fn start_edit(&self) -> Option<&CalendarDateTime> {
        self.start.as_ref()
    }

    /// The new end, if the edit moves or resizes the event.
    #[must_use]
    pub fn end_edit(&self) -> Option<&CalendarDateTime> {
        self.end.as_ref()
    }

    /// Whether this patch changes something the attendees must be told about — a move, a
    /// resize, or a new location (RFC 5546 §3.2.8). Retitling an event does not.
    ///
    /// An adapter that keeps an iTIP revision counter (CalDAV: `SEQUENCE`) bumps it exactly
    /// when this is `true`.
    #[must_use]
    pub fn is_significant(&self) -> bool {
        self.start.is_some()
            || self.end.is_some()
            || self.location.is_some()
            || self.recurrence.is_some()
    }

    /// Whether the patch changes nothing but its own stamp.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.summary.is_none()
            && self.description.is_none()
            && self.location.is_none()
            && self.start.is_none()
            && self.end.is_none()
            && self.recurrence.is_none()
    }
}

/// An edit of an already-stored event: the target it lands on, and what it changes.
///
/// Serializable, because it is the durable outbox payload. It records the **intent**, not
/// the rendered bytes — so a retry after a conflict re-applies it to a *freshly fetched*
/// base rather than re-sending a document built from a copy the server has moved past.
///
/// Construct it from the event as read ([`EventEdit::new`]), so the target it names and the
/// base it was computed against cannot drift apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEdit {
    /// The event being edited.
    pub event: EventId,
    /// Its cross-system `UID`, echoed on the receipt for reconciliation.
    pub uid: Uid,
    /// Which occurrence the patch lands on.
    pub target: PatchTarget,
    /// What the patch changes.
    pub patch: EventPatch,
}

impl EventEdit {
    /// An edit of `base`, the event as the caller read it.
    #[must_use]
    pub fn new(base: &Event, target: PatchTarget, patch: EventPatch) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            target,
            patch,
        }
    }
}

#[cfg(test)]
mod tests {
    use engine_core::time::{CalendarDate, LocalDateTime, TimeZoneId};

    use super::*;

    fn stamp() -> UtcDateTime {
        "2026-07-14T10:00:00Z".parse().unwrap()
    }

    fn zoned(local: &str) -> CalendarDateTime {
        CalendarDateTime::Zoned {
            local: local.parse::<LocalDateTime>().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        }
    }

    #[test]
    fn an_empty_patch_changes_nothing_but_its_stamp() {
        let patch = EventPatch::new(stamp());
        assert!(patch.is_empty());
        assert!(!patch.is_significant());
        assert_eq!(patch.stamp(), stamp());
        assert!(patch.summary_edit().is_none());
        assert!(patch.description_edit().is_none());
    }

    #[test]
    fn clearing_a_text_property_is_not_the_same_as_leaving_it_alone() {
        // The three states an optional text property has. Collapsing "clear" into "not
        // mentioned" would make a deletion silently un-deletable.
        let untouched = EventPatch::new(stamp());
        let set = EventPatch::new(stamp()).description("notes");
        let cleared = EventPatch::new(stamp()).clear_description();

        assert!(untouched.description_edit().is_none());
        assert_eq!(
            set.description_edit(),
            Some(&TextEdit::Set("notes".to_owned()))
        );
        assert_eq!(cleared.description_edit(), Some(&TextEdit::Clear));
        assert!(!cleared.is_empty());
    }

    #[test]
    fn only_a_move_resize_or_relocation_is_significant() {
        // What bumps an iTIP SEQUENCE: the things an attendee must be re-told (RFC 5546
        // §3.2.8). A retitle is not one of them.
        assert!(!EventPatch::new(stamp()).summary("Renamed").is_significant());
        assert!(
            !EventPatch::new(stamp())
                .description("notes")
                .is_significant()
        );
        assert!(
            EventPatch::new(stamp())
                .start(zoned("2026-08-01T09:00:00"))
                .is_significant()
        );
        assert!(
            EventPatch::new(stamp())
                .end(zoned("2026-08-01T10:00:00"))
                .is_significant()
        );
        assert!(EventPatch::new(stamp()).location("Room A").is_significant());
        assert!(EventPatch::new(stamp()).clear_location().is_significant());
    }

    #[test]
    fn an_edit_and_its_patch_survive_the_durable_payload_round_trip() {
        // The outbox stores this as JSON before the network call, so a restart must be able
        // to read back the exact intent — target included.
        let patch = EventPatch::new(stamp())
            .summary("Sprint planning")
            .clear_location()
            .start(zoned("2026-08-01T09:00:00"));
        let target = PatchTarget::Instance(Occurrence::starting(CalendarDateTime::Date(
            CalendarDate::new(2026, 8, 1).unwrap(),
        )));

        let encoded = serde_json::to_value(&patch).unwrap();
        assert_eq!(
            serde_json::from_value::<EventPatch>(encoded).unwrap(),
            patch
        );

        let encoded = serde_json::to_value(&target).unwrap();
        assert_eq!(
            serde_json::from_value::<PatchTarget>(encoded).unwrap(),
            target
        );
    }
}
