//! Graph structured `patternedRecurrence` → the engine's structural [`Recurrence`].
//!
//! Graph expresses recurrence as a `pattern` (`type` + `interval` + `daysOfWeek`/
//! `dayOfMonth`/`index`/`month`) plus a `range` (`endDate`/`noEnd`/`numbered`), not as
//! an `RRULE` string (`calendar-semantics.md`). This maps that structured form onto the
//! same [`RecurrenceRule`] the engine expander consumes, so a Graph series expands
//! locally through the bundled tzdb like every other provider's.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::calendar::{
    Frequency, NDay, Recurrence, RecurrenceBound, RecurrenceRule, Weekday,
};
use serde_json::Value;

use crate::{error::GraphError, json::opt_str};

/// Maps a Graph event's `recurrence` (`patternedRecurrence`) into a [`Recurrence`], or
/// `None` when the event is not a recurring series (a `singleInstance` has none).
pub(crate) fn parse_recurrence(value: &Value) -> Result<Option<Recurrence>, GraphError> {
    let Some(recur) = value.get("recurrence").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let pattern = recur
        .get("pattern")
        .ok_or_else(|| GraphError::protocol("recurrence has no pattern"))?;
    let range = recur
        .get("range")
        .ok_or_else(|| GraphError::protocol("recurrence has no range"))?;
    let mut recurrence = Recurrence::default();
    recurrence.rules.push(parse_rule(pattern, range)?);
    Ok(Some(recurrence))
}

/// Builds one [`RecurrenceRule`] from a Graph `pattern` + `range`. The `by*` fields
/// depend on the pattern `type` (Graph splits monthly/yearly into `absolute*` (day-of-
/// month) and `relative*` (nth-weekday, from `index`)), so frequency and `by*` are set
/// together in one match.
fn parse_rule(pattern: &Value, range: &Value) -> Result<RecurrenceRule, GraphError> {
    let ptype = opt_str(pattern, "type")
        .ok_or_else(|| GraphError::protocol("recurrence pattern has no type"))?;
    let mut rule = match ptype {
        "daily" => RecurrenceRule::new(Frequency::Daily),
        "weekly" => {
            let mut rule = RecurrenceRule::new(Frequency::Weekly);
            rule.by_day = days_of_week(pattern, None)?;
            rule
        }
        "absoluteMonthly" => {
            let mut rule = RecurrenceRule::new(Frequency::Monthly);
            rule.by_month_day = day_of_month(pattern);
            rule
        }
        "relativeMonthly" => {
            let mut rule = RecurrenceRule::new(Frequency::Monthly);
            rule.by_day = days_of_week(pattern, index_nth(pattern))?;
            rule
        }
        "absoluteYearly" => {
            let mut rule = RecurrenceRule::new(Frequency::Yearly);
            rule.by_month = month_of(pattern);
            rule.by_month_day = day_of_month(pattern);
            rule
        }
        "relativeYearly" => {
            let mut rule = RecurrenceRule::new(Frequency::Yearly);
            rule.by_month = month_of(pattern);
            rule.by_day = days_of_week(pattern, index_nth(pattern))?;
            rule
        }
        other => {
            return Err(GraphError::protocol(format!(
                "unknown recurrence pattern type {other:?}"
            )));
        }
    };

    if let Some(interval) = pattern
        .get("interval")
        .and_then(Value::as_u64)
        .and_then(|i| u32::try_from(i).ok())
        .and_then(NonZeroU32::new)
    {
        rule.interval = interval;
    }
    if let Some(first_day) = opt_str(pattern, "firstDayOfWeek").and_then(graph_weekday) {
        rule.first_day_of_week = first_day;
    }
    rule.bound = parse_bound(range)?;
    Ok(rule)
}

/// The `daysOfWeek` array as `NDay`s, each carrying `nth` (the pattern `index`, for the
/// relative monthly/yearly forms; `None` for weekly).
fn days_of_week(pattern: &Value, nth: Option<NonZeroI32>) -> Result<Vec<NDay>, GraphError> {
    let Some(days) = pattern.get("daysOfWeek").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    days.iter()
        .filter_map(Value::as_str)
        .map(|name| {
            graph_weekday(name)
                .map(|day| NDay {
                    day,
                    nth_of_period: nth,
                })
                .ok_or_else(|| GraphError::protocol(format!("unknown weekday {name:?}")))
        })
        .collect()
}

/// The `dayOfMonth` as a `BYMONTHDAY` list (Graph uses `0` for "not applicable").
fn day_of_month(pattern: &Value) -> Vec<i32> {
    match pattern.get("dayOfMonth").and_then(Value::as_i64) {
        Some(d) if d != 0 => i32::try_from(d).map(|d| vec![d]).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The `month` (1–12) as a `BYMONTH` list; the engine stores months as strings.
fn month_of(pattern: &Value) -> Vec<String> {
    match pattern.get("month").and_then(Value::as_u64) {
        Some(m) if (1..=12).contains(&m) => vec![m.to_string()],
        _ => Vec::new(),
    }
}

/// The `index` (`first`..`fourth`, `last`) as an nth-of-period: 1–4, or −1 for `last`.
fn index_nth(pattern: &Value) -> Option<NonZeroI32> {
    let nth = match opt_str(pattern, "index")? {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "last" => -1,
        _ => return None,
    };
    NonZeroI32::new(nth)
}

/// Maps a Graph `range` onto the rule's [`RecurrenceBound`]: `noEnd` → unbounded,
/// `numbered` → a count, `endDate` → an `UNTIL` at the end of the last day.
fn parse_bound(range: &Value) -> Result<RecurrenceBound, GraphError> {
    match opt_str(range, "type") {
        Some("noEnd") | None => Ok(RecurrenceBound::Unbounded),
        Some("numbered") => range
            .get("numberOfOccurrences")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .and_then(NonZeroU32::new)
            .map(RecurrenceBound::Count)
            .ok_or_else(|| GraphError::protocol("numbered range has no numberOfOccurrences")),
        Some("endDate") => {
            let end = opt_str(range, "endDate")
                .ok_or_else(|| GraphError::protocol("endDate range has no endDate"))?;
            // `endDate` is the last *date* an occurrence may start; UNTIL is an instant,
            // so bound at the end of that day to include an occurrence on it.
            let until = format!("{end}T23:59:59").parse().map_err(|e| {
                GraphError::protocol(format!("bad recurrence endDate {end:?}: {e}"))
            })?;
            Ok(RecurrenceBound::Until(until))
        }
        Some(other) => Err(GraphError::protocol(format!(
            "unknown recurrence range type {other:?}"
        ))),
    }
}

/// Maps a Graph weekday name (`"monday"`) to the engine [`Weekday`]. Graph uses full
/// lower-case names; the engine's own wire form is the two-letter `mo`/`tu`/…, so this
/// cannot go through serde.
fn graph_weekday(name: &str) -> Option<Weekday> {
    Some(match name {
        "monday" => Weekday::Mo,
        "tuesday" => Weekday::Tu,
        "wednesday" => Weekday::We,
        "thursday" => Weekday::Th,
        "friday" => Weekday::Fr,
        "saturday" => Weekday::Sa,
        "sunday" => Weekday::Su,
        _ => return None,
    })
}

#[cfg(test)]
#[path = "cal_recur_tests.rs"]
mod tests;
