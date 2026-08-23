//! The structural iCalendar patcher: edit a stored event **in place**, changing only
//! the properties the user actually changed.
//!
//! This is how CalDAV applies the neutral [`EventPatch`], and it exists because CalDAV's
//! write verb is `PUT` — replace the whole resource. There is no partial write, so the
//! *client* has to do the surgery, and rebuilding the document from the engine's projection
//! would be data loss. The projection is deliberately lossy (`calendar-semantics.md`) — it
//! has no room for the `RRULE`'s `BYSETPOS`, the `ATTENDEE`s' `DELEGATED-FROM`, the
//! `VALARM`s, the embedded `VTIMEZONE`, the `X-` properties another client wrote — so
//! re-serializing it to move an event by half an hour silently deletes every one of them
//! from the user's calendar. That is a save that looks like it worked.
//!
//! So [`patch_event_ical`] takes the stored [`RawIcal`], applies the patch, and returns a
//! document in which *every byte the patch did not touch is the byte that was there before*
//! — the original folding, the original line terminators, the properties this crate has
//! never heard of. The line surgery is [`lines`](super::lines); the component scan is
//! [`vevent`]; the property rules are [`plan`].
//!
//! **This machinery is CalDAV's alone, which is why it lives here.** A transport whose
//! update verb is already a patch — JMAP `CalendarEvent/set`, whose `update` takes a
//! JSON-pointer PatchObject — has the *server* do the surgery, and has no use for RFC 5545
//! line folding, `DTEND`-vs-`DURATION` exclusion or `SEQUENCE` bookkeeping. Only the
//! **intent** ([`EventPatch`], [`PatchTarget`]) is neutral, and that lives in
//! `engine-provider`.
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
//!   the series is unaffected. Splitting is **this crate's chore**: a JMAP server materializes the
//!   override itself, which is why the neutral `PatchTarget::Instance` does not promise the
//!   start/end that a split needs here.
//!
//! Removing one occurrence is [`exclude_occurrence_ical`], and it is here for the same
//! reason: there is no per-occurrence resource to `DELETE`, so it too is line surgery on the
//! series.
//!
//! `THISANDFUTURE` (splitting a series at a point) is **not** implemented; it needs the
//! master's `RRULE` rewritten with an `UNTIL`, which is a different operation from this
//! one (`calendar-semantics.md` lists the `RECURRENCE-ID` range semantics as staged).

mod plan;
mod vevent;

#[cfg(test)]
mod exclude_tests;
#[cfg(test)]
mod guard_tests;
#[cfg(test)]
mod patch_tests;
#[cfg(test)]
mod test_support;

use engine_core::{
    calendar::{RecurrenceBound, UntilForm, format_rrule},
    raw::RawIcal,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_provider::{DraftRecurrence, EventPatch, PatchTarget};

use super::{
    format::date_time_line,
    lines::{Document, Edit, Edits},
};
use crate::error::IcalError;

/// Applies `patch` to the `VEVENT` named by `target` in a stored calendar object
/// resource, returning the document to `PUT` back under `If-Match`.
///
/// Every line the patch does not touch — including properties this crate does not model,
/// the folding of long lines, and the document's line terminators — is preserved
/// byte-for-byte. See the module docs for what `target` means on a recurring event.
///
/// # Errors
///
/// Returns [`IcalError`] if the resource has no `VEVENT` or no master `VEVENT`
/// to patch; if the event has no `DTSTART`; if a new `DTSTART`/`DTEND` would change the
/// event's time form (a zoned or all-day event must not be silently converted); or if
/// [`PatchTarget::Instance`] targets an event that does not recur.
pub fn patch_event_ical(
    ical: &RawIcal,
    target: &PatchTarget,
    patch: &EventPatch,
) -> Result<RawIcal, IcalError> {
    let text = ical.as_str();
    let doc = Document::parse(text);
    let resource = vevent::scan(&doc)?;
    let mut edits = Edits::new();

    match target {
        PatchTarget::Series => {
            let master = resource.master(&doc)?;
            plan::plan(&doc, master, patch, &mut edits)?;
            plan::set_recurrence(&doc, master, &resource, patch, &mut edits)?;
        }
        PatchTarget::Instance(occurrence) => {
            // A single occurrence has no rule of its own — it is one instance *of* a rule
            // — so there is nothing a recurrence edit could mean here, and guessing would
            // either rewrite the whole series or write an RRULE onto an override.
            if patch.recurrence_edit().is_some() {
                return Err(IcalError::new(
                    "a recurrence edit targets the series, never one occurrence; an \
                     occurrence has no rule of its own",
                ));
            }
            let recurrence_id = &occurrence.start;
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

/// Removes **one occurrence** from a stored series, returning the document to `PUT` back
/// under `If-Match`.
///
/// CalDAV has no per-occurrence resource to `DELETE`: an occurrence is not a stored object,
/// the series is. So removing one is an edit of the series — an `EXDATE` naming it (RFC 5545
/// §3.8.5.1), and the loss of any `RECURRENCE-ID` override the user had made to it. Every
/// other byte is preserved, exactly as in [`patch_event_ical`].
///
/// # Errors
///
/// Returns [`IcalError`] if the resource has no `VEVENT` or no master `VEVENT`; if the event
/// does not recur (there is no occurrence to remove — delete the event); if it has no
/// `DTSTART`; or if `occurrence` is named in a different time form from the series.
pub fn exclude_occurrence_ical(
    ical: &RawIcal,
    occurrence: &CalendarDateTime,
    stamp: UtcDateTime,
) -> Result<RawIcal, IcalError> {
    let doc = Document::parse(ical.as_str());
    let resource = vevent::scan(&doc)?;
    let master = resource.master(&doc)?;
    let mut edits = Edits::new();
    plan::exclude(&doc, master, &resource, occurrence, stamp, &mut edits)?;
    Ok(RawIcal::new(doc.render(&edits)))
}

/// The `RRULE` value for a draft's recurrence, rendered in the `UNTIL` form the draft's
/// own `DTSTART` requires (RFC 5545 §3.3.10).
///
/// A zoned or UTC `DTSTART` obliges `UNTIL` to be UTC, and the instant that takes is the
/// caller's to resolve — this crate has no tzdata (`DraftRecurrence`). Refusing here is
/// the point: emitting the wall clock instead would end the series on a different day for
/// every reader outside the authoring zone.
pub(crate) fn rrule_value(
    recurrence: &DraftRecurrence,
    start: &CalendarDateTime,
) -> Result<String, IcalError> {
    let until = match (start, &recurrence.rule.bound) {
        // No UNTIL to render at all; the form is irrelevant.
        (_, RecurrenceBound::Unbounded | RecurrenceBound::Count(_))
        | (CalendarDateTime::Floating(_), RecurrenceBound::Until(_)) => UntilForm::Floating,
        (CalendarDateTime::Date(_), RecurrenceBound::Until(_)) => UntilForm::Date,
        (CalendarDateTime::Zoned { .. }, RecurrenceBound::Until(_)) => {
            let at = recurrence.until.ok_or_else(|| {
                IcalError::new(
                    "a recurrence ending at a wall clock on a zoned event needs that clock \
                     resolved to an instant: RFC 5545 requires UNTIL in UTC when DTSTART \
                     carries a TZID, and resolving it needs tzdata this crate does not have. \
                     Build the draft with DraftRecurrence::ending_at",
                )
            })?;
            UntilForm::Utc(at)
        }
    };
    format_rrule(&recurrence.rule, until).map_err(|e| IcalError::new(e.to_string()))
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
) -> Result<String, IcalError> {
    if !master.is_recurring(doc) {
        return Err(IcalError::new(
            "cannot override an instance of an event that does not recur; patch the series",
        ));
    }
    // The copy inherits the master's DTSTART/DTEND — which are the *first* occurrence's
    // times, not this one's. Deriving this occurrence's times would mean expanding the
    // recurrence rule, which this crate cannot do; so the caller — which is looking at
    // the occurrence it is editing, and holds its start and end — must state them. Left
    // to guess, the override would claim the series' opening slot. This requirement is
    // CalDAV's, not the neutral contract's: a JMAP server materializes the override from
    // the series itself and needs neither.
    if patch.start_edit().is_none() || patch.end_edit().is_none() {
        return Err(IcalError::new(
            "splitting a new override needs the occurrence's start and end on the patch \
             (the master's are the first occurrence's, not this one's); pass both, \
             unchanged if the edit does not move the event",
        ));
    }
    let start = master
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
    // The override's identity must be expressed like the series it overrides — a zoned
    // series is not overridden by a UTC RECURRENCE-ID naming "the same" moment.
    plan::ensure_same_form(&start, recurrence_id, "RECURRENCE-ID")?;

    let mut edits = Edits::new();
    plan::drop_series_rules(doc, master, &mut edits);
    // The RECURRENCE-ID goes before DTSTART — which the patch may itself be replacing, so
    // it is spliced in *before* that line rather than replacing it.
    let anchor = master
        .property(doc, "DTSTART")
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
    edits
        .entry(anchor)
        .or_default()
        .before
        .push_str(&doc.fold(anchor, &date_time_line("RECURRENCE-ID", recurrence_id)));
    plan::plan(doc, master, patch, &mut edits)?;
    Ok(doc.render_range(master.groups(), &edits))
}
