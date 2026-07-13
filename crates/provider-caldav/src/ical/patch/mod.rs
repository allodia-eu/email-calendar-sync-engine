//! The structural iCalendar patcher: edit a stored event **in place**, changing only
//! the properties the user actually changed.
//!
//! This is the update-path counterpart to [`build_event_ical`](super::build_event_ical),
//! and it exists because rebuilding a document from the engine's projection is data
//! loss. The projection is deliberately lossy (`calendar-semantics.md`) — it has no
//! room for the `RRULE`'s `BYSETPOS`, the `ATTENDEE`s' `DELEGATED-FROM`, the `VALARM`s,
//! the embedded `VTIMEZONE`, the `X-` properties another client wrote — so re-serializing
//! it to move an event by half an hour silently deletes every one of them from the user's
//! calendar. That is a save that looks like it worked. Hence the model invariant this
//! module implements: **writes round-trip from raw plus targeted patches, never by
//! re-serializing the projection**.
//!
//! So [`patch_event_ical`] takes the stored [`RawIcal`], applies an [`EventPatch`], and
//! returns a document in which *every byte the patch did not touch is the byte that was
//! there before* — the original folding, the original line terminators, the properties
//! this crate has never heard of. The line surgery is [`lines`](super::lines); the
//! component scan is [`vevent`]; the property rules are [`plan`].
//!
//! # Recurrence: whose event are you editing?
//!
//! [`PatchTarget`] has no default, deliberately. A drag on Tuesday's standup is either a
//! move of *that occurrence* or of *every Monday from now to eternity*, and only the user
//! knows which — so the caller must say, and the product UI must ask.
//!
//! - [`PatchTarget::Series`] patches the master `VEVENT`. Every occurrence moves.
//! - [`PatchTarget::Instance`] patches the `RECURRENCE-ID` override for one occurrence, **splitting
//!   a fresh one out of the master** if the series has never been overridden there: the master's
//!   `VEVENT` is copied (attendees, alarms, `X-` properties and all), its series-level
//!   `RRULE`/`RDATE`/`EXDATE` are dropped, a `RECURRENCE-ID` naming the occurrence's *original*
//!   start is added, and the patch lands on the copy. The master is left untouched, so the rest of
//!   the series is unaffected.
//!
//! `THISANDFUTURE` (splitting a series at a point) is **not** implemented; it needs the
//! master's `RRULE` rewritten with an `UNTIL`, which is a different operation from this
//! one (`calendar-semantics.md` lists the `RECURRENCE-ID` range semantics as staged).

mod plan;
mod vevent;

#[cfg(test)]
mod patch_tests;

use engine_core::{
    raw::RawIcal,
    time::{CalendarDateTime, UtcDateTime},
};

use super::{
    format::date_time_line,
    lines::{Document, Edit, Edits},
};
use crate::error::CalDavError;

/// Which `VEVENT` of a recurring resource an [`EventPatch`] lands on.
///
/// There is no default: see the module docs. For a non-recurring event only
/// [`PatchTarget::Series`] is meaningful (it is the resource's only `VEVENT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchTarget {
    /// The series master — the `VEVENT` with no `RECURRENCE-ID`. For a recurring event
    /// this edits **every** occurrence.
    Series,
    /// A single occurrence, named by its **original** start (the `RECURRENCE-ID`, RFC
    /// 5545 §3.8.4.4) — the start it had *before* this patch, which is its identity
    /// within the series, not where it is being moved to.
    ///
    /// The value must be in the master's own time form (a zoned series is overridden by
    /// a zoned `RECURRENCE-ID`), and it must name a real occurrence of the rule: this
    /// crate cannot expand recurrence to check that, so a caller that invents one writes
    /// an override that matches no instance.
    ///
    /// Splitting a **new** override additionally requires the patch to carry
    /// [`start`](EventPatch::start) **and** [`end`](EventPatch::end) — this occurrence's,
    /// not the series'. The master's are the *first* occurrence's times, and deriving
    /// this one's would mean expanding the rule, which this crate does not do. Pass them
    /// unchanged when the edit is not a move. (Patching an override that already exists
    /// needs neither: it already states its own times.)
    Instance(CalendarDateTime),
}

/// What an edit does to an optional TEXT property: give it a value, or take it away.
///
/// Distinct from *not mentioning it*, which leaves the property exactly as it was —
/// three states, so `Option<TextEdit>` rather than a nested `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TextEdit {
    /// Write this value, replacing any current one.
    Set(String),
    /// Delete the property.
    Clear,
}

/// The properties an edit changes. Anything not set here is left exactly as it was.
///
/// The [`stamp`](EventPatch::new) is supplied by the caller because engine time types
/// cannot read the system clock — the same reason
/// [`build_event_ical`](super::build_event_ical) derives one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPatch {
    /// The `DTSTAMP` (and `LAST-MODIFIED`, if the event has one) the revision carries.
    stamp: UtcDateTime,
    summary: Option<String>,
    description: Option<TextEdit>,
    location: Option<TextEdit>,
    start: Option<CalendarDateTime>,
    end: Option<CalendarDateTime>,
}

impl EventPatch {
    /// An empty patch stamped `stamp`. Applying it changes only `DTSTAMP` (and
    /// `LAST-MODIFIED` if present) — every other byte survives.
    #[must_use]
    pub fn new(stamp: UtcDateTime) -> Self {
        Self {
            stamp,
            summary: None,
            description: None,
            location: None,
            start: None,
            end: None,
        }
    }

    /// Sets the title (`SUMMARY`).
    #[must_use]
    pub fn summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    /// Sets the `DESCRIPTION`.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(TextEdit::Set(description.into()));
        self
    }

    /// Removes the `DESCRIPTION` — distinct from not mentioning it, which keeps it.
    #[must_use]
    pub fn clear_description(mut self) -> Self {
        self.description = Some(TextEdit::Clear);
        self
    }

    /// Sets the `LOCATION`.
    #[must_use]
    pub fn location(mut self, location: impl Into<String>) -> Self {
        self.location = Some(TextEdit::Set(location.into()));
        self
    }

    /// Removes the `LOCATION` — distinct from not mentioning it, which keeps it.
    #[must_use]
    pub fn clear_location(mut self) -> Self {
        self.location = Some(TextEdit::Clear);
        self
    }

    /// Moves the event's start (`DTSTART`).
    ///
    /// The value must keep the event's existing **form** — a zoned event stays in its own
    /// zone, an all-day event stays a date — or [`patch_event_ical`] errors rather than
    /// converting it. Shift the wall clock of the event's current start; do not resolve
    /// it to an instant first.
    #[must_use]
    pub fn start(mut self, start: CalendarDateTime) -> Self {
        self.start = Some(start);
        self
    }

    /// Moves the event's end (`DTEND`), under the same form rule as
    /// [`start`](Self::start).
    ///
    /// For an **all-day** event `DTEND` is *exclusive* (RFC 5545 §3.6.1): a one-day event
    /// on the 1st ends on the 2nd. Passing the last day instead silently shortens the
    /// event by a day.
    ///
    /// If the event states its end as a `DURATION` instead, that line is replaced by this
    /// `DTEND` — the two are mutually exclusive, so it cannot simply be added.
    #[must_use]
    pub fn end(mut self, end: CalendarDateTime) -> Self {
        self.end = Some(end);
        self
    }

    /// Whether this patch changes something attendees must be told about — a move, a
    /// resize, or a new location (RFC 5546 §3.2.8) — and therefore bumps `SEQUENCE`.
    /// Retitling an event does not.
    fn is_significant(&self) -> bool {
        self.start.is_some() || self.end.is_some() || self.location.is_some()
    }
}

/// Applies `patch` to the `VEVENT` named by `target` in a stored calendar object
/// resource, returning the document to `PUT` back under `If-Match`.
///
/// Every line the patch does not touch — including properties this crate does not model,
/// the folding of long lines, and the document's line terminators — is preserved
/// byte-for-byte. See the module docs for what `target` means on a recurring event.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the resource has no `VEVENT` or no master `VEVENT`
/// to patch; if the event has no `DTSTART`; if a new `DTSTART`/`DTEND` would change the
/// event's time form (a zoned or all-day event must not be silently converted); or if
/// [`PatchTarget::Instance`] targets an event that does not recur.
pub fn patch_event_ical(
    ical: &RawIcal,
    target: &PatchTarget,
    patch: &EventPatch,
) -> Result<RawIcal, CalDavError> {
    let text = ical.as_str();
    let doc = Document::parse(text);
    let resource = vevent::scan(&doc)?;
    let mut edits = Edits::new();

    match target {
        PatchTarget::Series => {
            let master = resource.master(&doc)?;
            plan::plan(&doc, master, patch, &mut edits)?;
        }
        PatchTarget::Instance(recurrence_id) => {
            if let Some(existing) = resource.override_for(&doc, recurrence_id) {
                // The series is already overridden here: patch that VEVENT in place.
                plan::plan(&doc, existing, patch, &mut edits)?;
            } else {
                // First edit to this occurrence: split a fresh override out of the master.
                let master = resource.master(&doc)?;
                let block = split_override(&doc, master, recurrence_id, patch)?;
                edits.insert(resource.splice_point()?, Edit::insert_before(block));
            }
        }
    }
    Ok(RawIcal::new(doc.render(&edits)))
}

/// Renders a new override `VEVENT` for the occurrence originally starting at
/// `recurrence_id`, by copying the master and patching the copy.
///
/// The copy is made from the master's **source bytes**, so the attendees, alarms and
/// `X-` properties come across exactly as they were, original folding included.
fn split_override(
    doc: &Document,
    master: &vevent::Vevent,
    recurrence_id: &CalendarDateTime,
    patch: &EventPatch,
) -> Result<String, CalDavError> {
    if !master.is_recurring(doc) {
        return Err(CalDavError::ical(
            "cannot override an instance of an event that does not recur; patch the series",
        ));
    }
    // The copy inherits the master's DTSTART/DTEND — which are the *first* occurrence's
    // times, not this one's. Deriving this occurrence's times would mean expanding the
    // recurrence rule, which this crate cannot do; so the caller — which is looking at
    // the occurrence it is editing, and holds its start and end — must state them. Left
    // to guess, the override would claim the series' opening slot.
    if patch.start.is_none() || patch.end.is_none() {
        return Err(CalDavError::ical(
            "splitting a new override needs the occurrence's start and end on the patch \
             (the master's are the first occurrence's, not this one's); pass both, \
             unchanged if the edit does not move the event",
        ));
    }
    let start = master
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| CalDavError::ical("event has no DTSTART"))?;
    // The override's identity must be expressed like the series it overrides — a zoned
    // series is not overridden by a UTC RECURRENCE-ID naming "the same" moment.
    plan::ensure_same_form(&start, recurrence_id, "RECURRENCE-ID")?;

    let mut edits = Edits::new();
    plan::drop_series_rules(doc, master, &mut edits);
    // The RECURRENCE-ID goes before DTSTART — which the patch may itself be replacing, so
    // it is spliced in *before* that line rather than replacing it.
    let anchor = master
        .property(doc, "DTSTART")
        .ok_or_else(|| CalDavError::ical("event has no DTSTART"))?;
    edits
        .entry(anchor)
        .or_default()
        .before
        .push_str(&doc.fold(anchor, &date_time_line("RECURRENCE-ID", recurrence_id)));
    plan::plan(doc, master, patch, &mut edits)?;
    Ok(doc.render_range(master.groups(), &edits))
}
