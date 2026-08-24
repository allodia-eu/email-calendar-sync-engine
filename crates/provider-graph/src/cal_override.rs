//! Reading the occurrences of a Graph series somebody changed or removed, and folding
//! them into the series they belong to.
//!
//! Graph states the two halves in two places. An occurrence that was **edited** comes back
//! as its own `calendarView` entry with `type: "exception"`, naming its series in
//! `seriesMasterId` and itself in `occurrenceId`. An occurrence that was **removed** has no
//! entry at all — it is a string in the series master's `cancelledOccurrences`, which the
//! delta does not carry, so [`crate::cal_fetch`] asks each master for it. Both land in the
//! same [`Recurrence::overrides`] map a CalDAV `RECURRENCE-ID` component and a JSCalendar
//! override land in, so the expander sees one representation whatever the transport was.
//!
//! # The date is in the event's own zone, which is why the master is read in it
//!
//! Both halves name an occurrence as `OID.<seriesMasterId>.<YYYY-MM-DD>`, and that date is
//! the occurrence's date in the zone the event was **authored** in. Measured: it does not
//! follow `Prefer: outlook.timezone`, while the event's `start` does — so a 23:30 Amsterdam
//! series read in Auckland starts on the 6th while its own ids still say the 5th. The map
//! is keyed by a wall clock in the zone the master is stored in, so the two must be the
//! same zone; [`crate::cal_fetch`] reads a series master in its own `originalStartTimeZone`
//! for exactly that reason. Nothing here can paper over a mismatch: engine-core carries no
//! tzdata, and a removed occurrence answers `404` at its derived id, so Graph will not
//! render its original start in any other zone.

use std::collections::BTreeMap;

use engine_core::{
    calendar::{Event, OverrideBuilder, Recurrence, RecurrenceOverride},
    ids::ProviderKey,
    time::{CalendarDate, CalendarDateTime, LocalDateTime},
};
use serde_json::Value;

use crate::{
    cal_normalize::parse_endpoint,
    error::GraphError,
    json::{bool_field, opt_str, req_str},
};

/// One occurrence-level entry, waiting for the master it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct PendingOverride {
    /// The series this overrides, from `seriesMasterId` (or the master that was asked for
    /// its cancellations).
    master: ProviderKey,
    /// The occurrence's original **date**, in the series' own zone. The time of day comes
    /// from the master at fold time — Graph's id carries no more than a date.
    on: CalendarDate,
    /// What the entry does to that occurrence.
    what: RecurrenceOverride,
}

/// Reads one `type: "exception"` entry as an override of its series.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if the entry has no `seriesMasterId`, no usable
/// `occurrenceId`, or times that will not parse.
pub(crate) fn pending_override(entry: &Value) -> Result<PendingOverride, GraphError> {
    let master = ProviderKey::new(req_str(entry, "seriesMasterId")?)
        .map_err(|e| GraphError::protocol(format!("bad seriesMasterId: {e}")))?;
    let on = occurrence_date(req_str(entry, "occurrenceId")?)?;
    Ok(PendingOverride {
        master,
        on,
        what: patch_of(entry)?,
    })
}

/// Reads a series master's `cancelledOccurrences` as one exclusion per entry.
///
/// # Errors
///
/// Returns [`GraphError::Protocol`] if an entry is not the `OID.<master>.<date>` form.
pub(crate) fn cancellations(
    master: &ProviderKey,
    doc: &Value,
) -> Result<Vec<PendingOverride>, GraphError> {
    doc.get("cancelledOccurrences")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|id| {
            Ok(PendingOverride {
                master: master.clone(),
                on: occurrence_date(id)?,
                // RFC 8984 makes an exclusion structural — it carries no patch — so a
                // cancellation says only that the occurrence is gone.
                what: RecurrenceOverride::Excluded,
            })
        })
        .collect()
}

/// The date out of an `OID.<seriesMasterId>.<YYYY-MM-DD>` occurrence id.
///
/// The master id is itself base64url-ish and carries `.`, so the date is the **last**
/// segment rather than the third.
fn occurrence_date(occurrence_id: &str) -> Result<CalendarDate, GraphError> {
    let date = occurrence_id
        .rsplit_once('.')
        .map(|(_, date)| date)
        .ok_or_else(|| {
            GraphError::protocol(format!("occurrence id {occurrence_id:?} has no date"))
        })?;
    date.parse().map_err(|e| {
        GraphError::protocol(format!("bad date in occurrence id {occurrence_id:?}: {e}"))
    })
}

/// The patch an overridden occurrence carries: where it is, how long it runs, what it is
/// called, its own notes and room, and whether the organizer called it off.
///
/// The field set is [`OverrideBuilder`]'s rather than this module's, so a changed occurrence
/// projects the same way whichever transport it arrived over. Graph states the whole event
/// rather than a patch, so what is read here is what the instance *is*.
fn patch_of(entry: &Value) -> Result<RecurrenceOverride, GraphError> {
    let all_day = bool_field(entry, "isAllDay");
    let start = parse_endpoint(entry, "start", all_day)?;
    let end = parse_endpoint(entry, "end", all_day)?;
    let duration = start
        .duration_until(&end)
        .map_err(|e| GraphError::protocol(format!("bad override start/end: {e}")))?;

    let mut builder = OverrideBuilder::new().start(&start).duration(duration);
    if let Some(title) = opt_str(entry, "subject") {
        builder = builder.title(title);
    }
    if let Some(notes) = notes_of(entry) {
        builder = builder.description(notes);
    }
    if let Some(room) = location_name(entry) {
        builder = builder.location_named(room);
    }
    if bool_field(entry, "isCancelled") {
        builder = builder.cancelled();
    }
    builder
        .build()
        .map_err(|e| GraphError::protocol(format!("bad override patch: {e}")))
}

/// The instance's own notes: the plain-text `body` when Graph sent text, else the
/// server-computed `bodyPreview` — the same rule the whole-event normalizer uses, so a
/// series and one of its occurrences describe themselves the same way.
fn notes_of(entry: &Value) -> Option<&str> {
    let body = entry.get("body");
    let text = if body.and_then(|b| opt_str(b, "contentType")) == Some("text") {
        body.and_then(|b| opt_str(b, "content"))
    } else {
        None
    };
    text.or_else(|| opt_str(entry, "bodyPreview"))
        .filter(|notes| !notes.is_empty())
}

/// The instance's own location name, from the singular `location` or the first of the
/// `locations` array Graph populates beside it.
fn location_name(entry: &Value) -> Option<&str> {
    entry
        .get("location")
        .and_then(|l| opt_str(l, "displayName"))
        .or_else(|| {
            entry
                .get("locations")
                .and_then(Value::as_array)
                .and_then(|list| list.first())
                .and_then(|l| opt_str(l, "displayName"))
        })
        .filter(|name| !name.is_empty())
}

/// Folds every collected override into the event it belongs to.
///
/// The key is the occurrence's date at the **master's** time of day, which is the
/// recurrence id the expander generates for it. An override whose master is not in this
/// pass is dropped: it cannot be keyed without one, and a delta that changes an occurrence
/// carries its master too (measured). An override reaching a non-recurring event is
/// dropped for the same reason CalDAV's is — a rule's exception with no rule is not
/// something this projection can state.
pub(crate) fn fold_into(events: &mut [Event], overrides: Vec<PendingOverride>) {
    if overrides.is_empty() {
        return;
    }
    let mut by_master: BTreeMap<ProviderKey, Vec<PendingOverride>> = BTreeMap::new();
    for pending in overrides {
        by_master
            .entry(pending.master.clone())
            .or_default()
            .push(pending);
    }
    for event in events {
        let Some(pending) = by_master.remove(event.id.key()) else {
            continue;
        };
        let at = local_of(&event.start);
        let recurrence = event.recurrence.get_or_insert_with(Recurrence::default);
        for entry in pending {
            let Ok(rid) = LocalDateTime::new(
                entry.on.year(),
                entry.on.month(),
                entry.on.day(),
                at.hour(),
                at.minute(),
                at.second(),
            ) else {
                continue;
            };
            recurrence.overrides.insert(rid, entry.what);
        }
    }
}

/// The wall clock of a scheduled value, with an all-day date read as its midnight — the
/// form [`Recurrence::overrides`] is keyed by.
fn local_of(value: &CalendarDateTime) -> LocalDateTime {
    match value {
        CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => *local,
        CalendarDateTime::Date(date) => {
            LocalDateTime::new(date.year(), date.month(), date.day(), 0, 0, 0)
                .unwrap_or_else(|_| unreachable!("a CalendarDate always holds a valid date"))
        }
    }
}

#[cfg(test)]
#[path = "cal_override_tests.rs"]
mod tests;
