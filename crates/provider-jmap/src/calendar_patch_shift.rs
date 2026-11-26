//! Moving a series moves the keys of its `recurrenceOverrides` with it.
//!
//! An entry is keyed by its occurrence's **original** start (RFC 8984 §4.3.3), so a series
//! whose start moves gives its occurrences other keys. Left at the old one, an entry names no
//! occurrence: the occurrence it was made for comes back, the entry is drawn as an added one
//! beside it, and an exclusion excludes nothing. A key cannot be renamed through a pointer, so
//! the map is written whole, each entry's own contents copied from the preserved raw: one the
//! user moved keeps the time they gave it, and one that only renamed its occurrence follows the
//! series, because a patch without a `start` takes the occurrence's.
//!
//! Which slots a move moves is `SeriesShift`'s to say, for every transport that keeps its
//! overrides through a series move.

use engine_core::{
    calendar::{Event, RecurrenceRule, SeriesShift},
    time::LocalDateTime,
};
use engine_provider::{EventPatch, RecurrenceEdit};
use serde_json::{Map, Value};

use crate::error::JmapError;

/// The series' `recurrenceOverrides` map with every key moved to where `patch` moves its slot,
/// or `None` when the patch moves no slot, or the series has no overrides to move.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the series has overrides but no preserved JSCalendar to
/// copy them from: writing them back from the projection would lose what it does not model.
pub(crate) fn moved_overrides(
    base: &Event,
    patch: &EventPatch,
) -> Result<Option<Value>, JmapError> {
    let Some(to) = patch.start_edit() else {
        return Ok(None);
    };
    let Some(recurrence) = base.recurrence.as_ref().filter(|r| !r.overrides.is_empty()) else {
        return Ok(None);
    };
    let rules: Vec<RecurrenceRule> = match patch.recurrence_edit() {
        // A rule taken away takes the series' occurrences with it; there is nothing to move.
        Some(RecurrenceEdit::Clear) => return Ok(None),
        Some(RecurrenceEdit::Set(draft)) => vec![draft.rule.clone()],
        None => recurrence.rules.clone(),
    };
    let Some(shift) = SeriesShift::between(&base.start, to, &rules) else {
        return Ok(None);
    };

    let raw = base.raw_jscalendar.as_ref().ok_or_else(|| {
        JmapError::protocol(
            "event has no preserved JSCalendar; re-sync it before moving the series",
        )
    })?;
    let value: Value = serde_json::from_str(raw.as_str()).map_err(|error| {
        JmapError::protocol(format!("preserved JSCalendar is not JSON: {error}"))
    })?;
    let Some(entries) = value.get("recurrenceOverrides").and_then(Value::as_object) else {
        return Ok(None);
    };
    let moved: Map<String, Value> = entries
        .iter()
        .map(|(key, entry)| (moved_key(key, shift), entry.clone()))
        .collect();
    Ok(Some(Value::Object(moved)))
}

/// `key` moved by `shift`; a key that does not parse, or would leave representable time, names
/// no slot of this series and is kept as it was.
fn moved_key(key: &str, shift: SeriesShift) -> String {
    key.parse::<LocalDateTime>()
        .ok()
        .and_then(|slot| shift.slot(slot))
        .and_then(|slot| serde_json::to_value(slot).ok())
        .and_then(|slot| slot.as_str().map(str::to_owned))
        .unwrap_or_else(|| key.to_owned())
}
