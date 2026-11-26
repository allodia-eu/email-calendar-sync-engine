//! Moving a series moves its overrides and exclusions with it.
//!
//! A `RECURRENCE-ID` and an `EXDATE` name an occurrence by its **original** start (RFC 5545
//! §3.8.4.4, §3.8.5.1), so a series whose start moves gives its occurrences other names. One
//! left at the old slot names nothing: an override is drawn as an added occurrence beside the
//! one it was made for, which comes back, and an exclusion excludes nothing, so a deleted
//! occurrence returns. Which slots a move moves is `SeriesShift`'s to say, the same answer JMAP
//! re-keys its `recurrenceOverrides` by.
//!
//! An override still at its slot (`DTSTART` equal to its `RECURRENCE-ID`) moves with it, as a
//! JSCalendar override without a `start` follows the series; one the user moved keeps the time
//! they gave it.

use engine_core::calendar::{RecurrenceRule, SeriesShift, parse_rrule};
use engine_provider::{EventPatch, RecurrenceEdit};

use super::{
    super::{
        format::date_time_line,
        lines::{Document, Edits, LineEdit},
        unfold::{content_lines, split_once_unquoted},
        value::parse_date_time_list,
    },
    vevent::{Resource, Vevent, property_name},
};
use crate::error::IcalError;

/// Queues the edits that move the series' exclusions and overrides to the slots `patch` moves
/// them to. Nothing, when the patch moves no slot.
///
/// # Errors
///
/// Returns [`IcalError`] if the master has no `DTSTART` to measure the move from.
pub(super) fn shift_slots(
    doc: &Document,
    master: &Vevent,
    resource: &Resource,
    patch: &EventPatch,
    edits: &mut Edits,
) -> Result<(), IcalError> {
    let Some(to) = patch.start_edit() else {
        return Ok(());
    };
    let rules = match patch.recurrence_edit() {
        // The rule goes, and its overrides with it (`set_recurrence`): there is nothing to move.
        Some(RecurrenceEdit::Clear) => return Ok(()),
        Some(RecurrenceEdit::Set(draft)) => vec![draft.rule.clone()],
        None => rules_of(doc, master),
    };
    let from = master
        .date_time(doc, "DTSTART")
        .transpose()?
        .ok_or_else(|| IcalError::new("event has no DTSTART"))?;
    let Some(shift) = SeriesShift::between(&from, to, &rules) else {
        return Ok(());
    };

    for &group in &master.own {
        let logical = doc.logical(group);
        if !property_name(&logical).eq_ignore_ascii_case("EXDATE") {
            continue;
        }
        let Some(line) = content_lines(&logical).into_iter().next() else {
            continue;
        };
        // One line per value: a list's values share one set of parameters, and a value that
        // would leave representable time names no occurrence and is dropped with the line.
        let moved: Vec<String> = parse_date_time_list(&line)
            .iter()
            .filter_map(|value| shift.value(value))
            .map(|value| date_time_line("EXDATE", &value))
            .collect();
        let Some((last, rest)) = moved.split_last() else {
            continue;
        };
        let edit = edits.entry(group).or_default();
        for extra in rest {
            edit.before.push_str(&doc.fold(group, extra));
        }
        edit.line = LineEdit::Replace(last.clone());
    }

    for vevent in resource.overrides(doc) {
        let Some(Ok(slot)) = vevent.date_time(doc, "RECURRENCE-ID") else {
            continue;
        };
        let at_its_slot = vevent
            .date_time(doc, "DTSTART")
            .and_then(Result::ok)
            .is_some_and(|start| start == slot);
        move_property(doc, vevent, edits, "RECURRENCE-ID", shift);
        if at_its_slot {
            move_property(doc, vevent, edits, "DTSTART", shift);
            move_property(doc, vevent, edits, "DTEND", shift);
        }
    }
    Ok(())
}

/// The series' own rules; one that will not parse names no slot and is left out, as the read
/// path leaves it out.
fn rules_of(doc: &Document, master: &Vevent) -> Vec<RecurrenceRule> {
    master
        .own
        .iter()
        .map(|&group| doc.logical(group))
        .filter(|logical| property_name(logical).eq_ignore_ascii_case("RRULE"))
        .filter_map(|logical| {
            let (_, value) = split_once_unquoted(&logical, ':')?;
            parse_rrule(value.trim()).ok()
        })
        .collect()
}

/// Rewrites `vevent`'s `name` date-time moved by `shift`, in the form it was written in.
fn move_property(
    doc: &Document,
    vevent: &Vevent,
    edits: &mut Edits,
    name: &str,
    shift: SeriesShift,
) {
    let (Some(group), Some(Ok(value))) = (vevent.property(doc, name), vevent.date_time(doc, name))
    else {
        return;
    };
    if let Some(moved) = shift.value(&value) {
        edits.entry(group).or_default().line = LineEdit::Replace(date_time_line(name, &moved));
    }
}
