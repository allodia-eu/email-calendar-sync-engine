//! Reading the entries Google returns for occurrences the user changed, and folding them
//! into the series they belong to.
//!
//! With `singleEvents=false` a series comes back as the **master** plus one entry per
//! occurrence somebody touched — each carrying `recurringEventId` (the master's id) and
//! `originalStartTime` (which occurrence), and a cancelled one carrying
//! `status: "cancelled"` as well. That is the same information CalDAV states as a
//! `RECURRENCE-ID` `VEVENT` and JSCalendar as a `recurrenceOverrides` entry, so it folds into
//! the same [`Recurrence::overrides`] map and the expander sees one representation.
//!
//! # Why the folding happens after the whole pass, not per entry
//!
//! An entry names its master by id, and nothing orders the two: the master may be on an
//! earlier page, a later one, or — on a delta — not present at all. So the entries are
//! collected as they are read and folded once every page is in.
//!
//! **A delta that changes an occurrence carries its master too**, measured: overriding one
//! occurrence and cancelling another bumped the master's `updated`, and all three arrived in
//! the next `syncToken` response. An entry whose master is nowhere in the pass is therefore
//! dropped rather than chased — the recovery would be two more requests (`events.get` for the
//! master's `iCalUID`, then `events.list?iCalUID=` for the series, which does return the
//! master and every override) for a case the server has not been seen to produce.

use std::collections::BTreeMap;

use engine_core::{
    calendar::{Event, Recurrence, RecurrenceOverride},
    ids::ProviderKey,
    patch::PatchObject,
    time::{CalendarDateTime, LocalDateTime},
};
use serde_json::Value;

use crate::{
    cal_normalize::parse_endpoint,
    error::GoogleError,
    json::{opt_str, req_str},
};

/// One occurrence-level entry, waiting for the master it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct PendingOverride {
    /// The series this overrides, from `recurringEventId`.
    master: ProviderKey,
    /// The occurrence's **original** start — the key it lands under, which is its identity
    /// within the series and not wherever it may have been moved to.
    at: LocalDateTime,
    /// What the entry does to that occurrence.
    what: RecurrenceOverride,
}

/// Reads one `recurringEventId` entry as an override of its series.
///
/// # Errors
///
/// Returns [`GoogleError::Protocol`] if the entry has no `recurringEventId`, no
/// `originalStartTime`, or times that will not parse.
pub(crate) fn pending_override(
    value: &Value,
    default_zone: Option<&str>,
) -> Result<PendingOverride, GoogleError> {
    let master = ProviderKey::new(req_str(value, "recurringEventId")?)
        .map_err(|e| GoogleError::protocol(format!("bad recurringEventId: {e}")))?;
    let original = parse_endpoint(value, "originalStartTime", default_zone)?;
    let at = local_of(&original);

    // A cancelled entry says only that the occurrence is gone. RFC 8984 makes that
    // structural — an excluded override carries no patch — so nothing else is read off it.
    if opt_str(value, "status") == Some("cancelled") {
        return Ok(PendingOverride {
            master,
            at,
            what: RecurrenceOverride::Excluded,
        });
    }
    Ok(PendingOverride {
        master,
        at,
        what: patch_of(value, default_zone)?,
    })
}

/// The patch an overridden occurrence carries: where it is, how long it runs, and what it is
/// called.
///
/// Deliberately the **same** field set `engine_ical`'s `override_patch` folds out of a
/// `RECURRENCE-ID` `VEVENT`, so the projection of a moved occurrence does not depend on which
/// transport it arrived over. Google states the whole event rather than a patch, so what is
/// read here is what the instance *is* — every one of these fields is present on it.
fn patch_of(value: &Value, default_zone: Option<&str>) -> Result<RecurrenceOverride, GoogleError> {
    let start = parse_endpoint(value, "start", default_zone)?;
    let end = parse_endpoint(value, "end", default_zone)?;
    let duration = start
        .duration_until(&end)
        .map_err(|e| GoogleError::protocol(format!("bad override start/end: {e}")))?;

    let mut fields = vec![(
        "start".to_owned(),
        Value::String(local_of(&start).to_string()),
    )];
    if let Some(zone) = start.zone() {
        fields.push((
            "timeZone".to_owned(),
            Value::String(zone.as_str().to_owned()),
        ));
    }
    fields.push(("duration".to_owned(), Value::String(duration.to_string())));
    if let Some(title) = opt_str(value, "summary") {
        fields.push(("title".to_owned(), Value::String(title.to_owned())));
    }
    PatchObject::new(fields)
        .map(RecurrenceOverride::Patch)
        .map_err(|e| GoogleError::protocol(format!("bad override patch: {e}")))
}

/// Folds every collected override into the event it belongs to.
///
/// An override reaches a **non-recurring** event only if the server contradicted itself, so
/// it is dropped with the rest: `Recurrence` describes a rule and its exceptions, and an
/// exception with no rule is not something this projection can state.
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
        let recurrence = event.recurrence.get_or_insert_with(Recurrence::default);
        for entry in pending {
            recurrence.overrides.insert(entry.at, entry.what);
        }
    }
}

/// The wall clock of a scheduled value, with an all-day date read as its midnight — the form
/// [`Recurrence::overrides`] is keyed by.
pub(crate) fn local_of(value: &CalendarDateTime) -> LocalDateTime {
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
