//! Turning an [`EventPatch`] into the set of line edits that express it.
//!
//! Every rule here exists to keep the edit *targeted*: only a property the caller
//! actually changed may produce a different byte, plus the bookkeeping RFC 5545
//! requires of a revised event (`DTSTAMP`, `LAST-MODIFIED`, `SEQUENCE`).

use engine_core::time::{CalendarDateTime, UtcDateTime};
use engine_provider::{EventPatch, RecurrenceEdit, TextEdit};

use super::{
    super::{
        format::{date_time_line, escape_text, format_utc},
        lines::{Document, Edits, LineEdit},
        unfold::split_once_unquoted,
    },
    vevent::{Resource, Vevent, property_name},
};
use crate::error::IcalError;

/// Plans every line edit `patch` implies for `vevent`, writing them into `edits`.
///
/// # Errors
///
/// Returns [`IcalError`] if the event has no `DTSTART` to move, or if a new
/// `DTSTART`/`DTEND` would change the value's *form* (see [`ensure_same_form`]).
pub(super) fn plan(
    doc: &Document,
    vevent: &Vevent,
    patch: &EventPatch,
    edits: &mut Edits,
) -> Result<(), IcalError> {
    // The start the end is validated against: the new one if this patch moves it,
    // else the one already on the event.
    let current_start = vevent
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
    // Check the start's form before anything downstream reads it: an end validated
    // against a start that is itself illegal reports the wrong property.
    if let Some(start) = patch.start_edit() {
        ensure_same_form(&current_start, start, "DTSTART")?;
    }
    let effective_start = patch
        .start_edit()
        .cloned()
        .unwrap_or_else(|| current_start.clone());

    // The end the event will have afterwards: the new one, else the one it already has.
    // Validating against *that* catches the caller who moves the start past the existing
    // end without resizing — an inversion neither line is individually wrong about.
    let effective_end = match patch.end_edit() {
        Some(end) => Some(end.clone()),
        None => vevent.date_time(doc, "DTEND").transpose()?,
    };
    if let Some(end) = &effective_end {
        ensure_same_form(&effective_start, end, "DTEND")?;
        // An end before its start is never what the user meant, and it is worse than
        // useless downstream: the reader rejects the event as malformed and drops it, so
        // the edit looks saved and the event disappears. Refuse it here, where the caller
        // can still be told.
        effective_start.duration_until(end).map_err(|_| {
            IcalError::new(
                "the edit would leave DTEND before DTSTART; an event cannot end before it begins",
            )
        })?;
    }

    if let Some(start) = patch.start_edit() {
        let group = vevent
            .property(doc, "DTSTART")
            .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
        replace(edits, group, date_time_line("DTSTART", start));
    }
    if let Some(end) = patch.end_edit() {
        set_end(doc, vevent, edits, end);
    }
    if let Some(summary) = patch.summary_edit() {
        set_text(
            doc,
            vevent,
            edits,
            "SUMMARY",
            &TextEdit::Set(summary.to_owned()),
        );
    }
    if let Some(description) = patch.description_edit() {
        set_text(doc, vevent, edits, "DESCRIPTION", description);
    }
    if let Some(location) = patch.location_edit() {
        set_text(doc, vevent, edits, "LOCATION", location);
    }

    revise(doc, vevent, edits, patch.stamp(), patch.is_significant());
    Ok(())
}

/// The bookkeeping RFC 5545 requires of a revised event.
///
/// A fresh `DTSTAMP` (§3.8.7.2), and a `LAST-MODIFIED` that would otherwise be a lie — but
/// only if the event kept one. CalDAV is a client-stamped transport: the caller's stamp is
/// what lands. A change attendees care about also bumps `SEQUENCE` (§3.8.7.4).
fn revise(
    doc: &Document,
    vevent: &Vevent,
    edits: &mut Edits,
    stamp: UtcDateTime,
    significant: bool,
) {
    set_or_insert(doc, vevent, edits, "DTSTAMP", &format_utc(stamp));
    if let Some(group) = vevent.property(doc, "LAST-MODIFIED") {
        replace(edits, group, format!("LAST-MODIFIED:{}", format_utc(stamp)));
    }
    if significant {
        bump_sequence(doc, vevent, edits);
    }
}

/// Plans the removal of one occurrence from a series: an `EXDATE` naming it, and the loss of
/// any override the user had made to it.
///
/// The exclusion goes in as its **own** `EXDATE` line rather than being merged into one the
/// event already carries. The property may repeat (RFC 5545 §3.8.5.1), and merging would
/// mean rewriting a line whose `TZID` and value list this edit has no business touching —
/// against a patcher whose whole promise is that untouched bytes stay untouched.
///
/// The override has to go with it, for the same reason clearing the rule takes the overrides
/// (see [`Resource::overrides`]): an override on an instant the rule no longer produces is
/// not inert, it is an *extra* occurrence. Left behind, the occurrence the user just deleted
/// would keep being drawn.
///
/// # Errors
///
/// Returns [`IcalError`] if the event does not recur (there is no occurrence to remove, only
/// the event), if it has no `DTSTART`, or if the occurrence is named in a different time
/// form from the series — which would name no instance of it.
pub(super) fn exclude(
    doc: &Document,
    master: &Vevent,
    resource: &Resource,
    occurrence: &CalendarDateTime,
    stamp: UtcDateTime,
    edits: &mut Edits,
) -> Result<(), IcalError> {
    if !master.is_recurring(doc) {
        return Err(IcalError::new(
            "cannot remove one occurrence of an event that does not recur; delete the event",
        ));
    }
    let start = master
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
    ensure_same_form(&start, occurrence, "EXDATE")?;

    insert(doc, master, edits, &date_time_line("EXDATE", occurrence));
    if let Some(overridden) = resource.override_for(doc, occurrence) {
        for group in overridden.groups() {
            remove(edits, group);
        }
    }
    // Cancelling an occurrence is exactly the kind of change an attendee needs to hear about.
    revise(doc, master, edits, stamp, true);
    Ok(())
}

/// Applies a recurrence edit to the series master.
///
/// Separate from [`plan`] because a *removal* reaches past the master's own lines: the
/// override `VEVENT`s have to go with the rule (see [`Resource::overrides`]), and only the
/// whole resource can see them.
///
/// A **replacement** deliberately touches nothing but the `RRULE` line. `EXDATE`, `RDATE`
/// and the override components record what the *user* did to individual occurrences, and on
/// this transport those survive a rule change (`calendar-semantics.md`); wiping them would
/// be Microsoft Graph's behaviour, imposed on a server that does not have it.
///
/// # Errors
///
/// Returns [`IcalError`] if the rule cannot be rendered as an `RRULE` — a non-Gregorian
/// rule, or an `UNTIL` on a zoned series with no resolved instant.
pub(super) fn set_recurrence(
    doc: &Document,
    master: &Vevent,
    resource: &Resource,
    patch: &EventPatch,
    edits: &mut Edits,
) -> Result<(), IcalError> {
    let Some(edit) = patch.recurrence_edit() else {
        return Ok(());
    };
    let start = master
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;

    match edit {
        RecurrenceEdit::Set(recurrence) => {
            let line = format!("RRULE:{}", super::rrule_value(recurrence, &start)?);
            match master.property(doc, "RRULE") {
                Some(group) => replace(edits, group, line),
                None => insert(doc, master, edits, &line),
            }
        }
        RecurrenceEdit::Clear => {
            drop_series_rules(doc, master, edits);
            for group in master.own.iter().copied() {
                let name = property_name(&doc.logical(group)).to_ascii_uppercase();
                if name == "EXRULE" {
                    remove(edits, group);
                }
            }
            for override_vevent in resource.overrides(doc) {
                for group in override_vevent.groups() {
                    remove(edits, group);
                }
            }
        }
    }
    Ok(())
}

/// Writes the event's end, respecting how the event already expresses it.
///
/// `DTEND` and `DURATION` are mutually exclusive (RFC 5545 §3.6.1), so an event that
/// states its end as a `DURATION` has that line **replaced** by the new `DTEND` rather
/// than gaining a second, contradictory end. That is the one property form this patcher
/// changes, and only because emitting both would be malformed.
fn set_end(doc: &Document, vevent: &Vevent, edits: &mut Edits, end: &CalendarDateTime) {
    let line = date_time_line("DTEND", end);
    if let Some(group) = vevent
        .property(doc, "DTEND")
        .or_else(|| vevent.property(doc, "DURATION"))
    {
        replace(edits, group, line);
    } else {
        insert(doc, vevent, edits, &line);
    }
}

/// Writes or deletes a TEXT property.
///
/// An existing line keeps its **parameters** (`LANGUAGE`, `ALTREP`, …) — only the value
/// after the unquoted `:` is rewritten, so the edit stays as small as the change.
fn set_text(doc: &Document, vevent: &Vevent, edits: &mut Edits, name: &str, value: &TextEdit) {
    match (vevent.property(doc, name), value) {
        (Some(group), TextEdit::Set(text)) => {
            let logical = doc.logical(group);
            let head = split_once_unquoted(&logical, ':').map_or(name, |(head, _)| head);
            replace(edits, group, format!("{head}:{}", escape_text(text)));
        }
        (Some(group), TextEdit::Clear) => remove(edits, group),
        (None, TextEdit::Set(text)) => {
            insert(doc, vevent, edits, &format!("{name}:{}", escape_text(text)));
        }
        (None, TextEdit::Clear) => {}
    }
}

/// Replaces a property that must exist afterwards, whether or not it exists now.
fn set_or_insert(doc: &Document, vevent: &Vevent, edits: &mut Edits, name: &str, value: &str) {
    let line = format!("{name}:{value}");
    match vevent.property(doc, name) {
        Some(group) => replace(edits, group, line),
        None => insert(doc, vevent, edits, &line),
    }
}

/// Increments `SEQUENCE` — the revision counter an organizer bumps on a change that
/// matters to attendees (RFC 5545 §3.8.7.4, RFC 5546 §3.2.8). An absent `SEQUENCE` is
/// `0`, so the first bump writes `1`.
fn bump_sequence(doc: &Document, vevent: &Vevent, edits: &mut Edits) {
    let current = vevent
        .property(doc, "SEQUENCE")
        .and_then(|group| {
            let logical = doc.logical(group);
            let (_, value) = split_once_unquoted(&logical, ':')?;
            value.trim().parse::<u32>().ok()
        })
        .unwrap_or(0);
    set_or_insert(
        doc,
        vevent,
        edits,
        "SEQUENCE",
        &current.saturating_add(1).to_string(),
    );
}

/// Rejects a new `DTSTART`/`DTEND` whose value *form* differs from the event's — the
/// iCalendar half of [`CalendarDateTime::has_same_form`], which states the rule and why it
/// is universal.
pub(super) fn ensure_same_form(
    current: &CalendarDateTime,
    new: &CalendarDateTime,
    name: &str,
) -> Result<(), IcalError> {
    if current.has_same_form(new) {
        return Ok(());
    }
    Err(IcalError::new(format!(
        "{name} would change the event's time form ({}), which a move must never do \
         silently; supply the new value in the event's own form",
        current.form_name(),
    )))
}

/// Queues a replacement for one logical line, leaving anything already spliced in
/// before it intact.
fn replace(edits: &mut Edits, group: usize, line: String) {
    edits.entry(group).or_default().line = LineEdit::Replace(line);
}

/// Queues the removal of one logical line, leaving anything already spliced in before
/// it intact.
fn remove(edits: &mut Edits, group: usize) {
    edits.entry(group).or_default().line = LineEdit::Remove;
}

/// Queues a **new** property line, folded, at the event's insert anchor — before its
/// first sub-component, so a property never lands after a `VALARM`.
fn insert(doc: &Document, vevent: &Vevent, edits: &mut Edits, line: &str) {
    let anchor = vevent.anchor;
    edits
        .entry(anchor)
        .or_default()
        .before
        .push_str(&doc.fold(anchor, line));
}

/// Drops the series-level properties when splitting one occurrence out of a master:
/// an override describes a single instance, so it must not carry the rule that
/// generates the series (RFC 5545 §3.8.5).
pub(super) fn drop_series_rules(doc: &Document, master: &Vevent, edits: &mut Edits) {
    for &group in &master.own {
        let logical = doc.logical(group);
        let name = property_name(&logical).to_ascii_uppercase();
        if matches!(name.as_str(), "RRULE" | "RDATE" | "EXDATE") {
            remove(edits, group);
        }
    }
}
