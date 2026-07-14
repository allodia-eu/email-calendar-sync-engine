//! Turning an [`EventPatch`] into the set of line edits that express it.
//!
//! Every rule here exists to keep the edit *targeted*: only a property the caller
//! actually changed may produce a different byte, plus the bookkeeping RFC 5545
//! requires of a revised event (`DTSTAMP`, `LAST-MODIFIED`, `SEQUENCE`).

use engine_core::time::CalendarDateTime;
use engine_provider::{EventPatch, TextEdit};

use super::{
    super::{
        format::{date_time_line, escape_text, format_utc},
        lines::{Document, Edits, LineEdit},
        unfold::split_once_unquoted,
    },
    vevent::{Vevent, property_name},
};
use crate::error::CalDavError;

/// Plans every line edit `patch` implies for `vevent`, writing them into `edits`.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the event has no `DTSTART` to move, or if a new
/// `DTSTART`/`DTEND` would change the value's *form* (see [`ensure_same_form`]).
pub(super) fn plan(
    doc: &Document,
    vevent: &Vevent,
    patch: &EventPatch,
    edits: &mut Edits,
) -> Result<(), CalDavError> {
    // The start the end is validated against: the new one if this patch moves it,
    // else the one already on the event.
    let current_start = vevent
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| CalDavError::ical("event has no DTSTART"))?;
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
            CalDavError::ical(
                "the edit would leave DTEND before DTSTART; an event cannot end before it begins",
            )
        })?;
    }

    if let Some(start) = patch.start_edit() {
        let group = vevent
            .property(doc, "DTSTART")
            .ok_or_else(|| CalDavError::ical("event has no DTSTART"))?;
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

    // A revised instance carries a fresh DTSTAMP (RFC 5545 §3.8.7.2), and a
    // LAST-MODIFIED that would otherwise be a lie — but only if the event kept one.
    // CalDAV is a client-stamped transport: the caller's stamp is what lands.
    set_or_insert(doc, vevent, edits, "DTSTAMP", &format_utc(patch.stamp()));
    if let Some(group) = vevent.property(doc, "LAST-MODIFIED") {
        replace(
            edits,
            group,
            format!("LAST-MODIFIED:{}", format_utc(patch.stamp())),
        );
    }
    if patch.is_significant() {
        bump_sequence(doc, vevent, edits);
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
) -> Result<(), CalDavError> {
    if current.has_same_form(new) {
        return Ok(());
    }
    Err(CalDavError::ical(format!(
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
