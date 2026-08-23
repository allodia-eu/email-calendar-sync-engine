//! The engine's structural [`RecurrenceRule`] → Graph `patternedRecurrence`.
//!
//! The inverse of [`cal_recur`](super::cal_recur), and the write half of Graph's recurrence
//! support: Graph takes a `pattern` + `range` object rather than an `RRULE` string
//! (`calendar-semantics.md`), so a create or a rule edit renders the engine rule here
//! instead of going through `engine_core::calendar::format_rrule`.
//!
//! # Graph's pattern set is narrower than `RRULE`, so this refuses rather than approximates
//!
//! `RRULE` can say things Graph's six pattern types cannot — every Monday of a month with no
//! ordinal, two different days-of-month, a `BYSETPOS`, a sub-daily frequency. Rendering those
//! as the nearest Graph pattern would put a **different series** on the user's calendar and
//! report success, so each is an [`GraphError`] naming what could not be expressed. The
//! product core checks a rule against the expander's supported subset before it ever gets
//! here; this is the second gate, at the boundary that actually writes.
//!
//! # What Graph requires that the rule leaves implicit
//!
//! `FREQ=MONTHLY` with no `BY*` part means "the same day of the month as `DTSTART`", and
//! `FREQ=YEARLY` means "the same day and month". Graph has no such shorthand — its
//! `absoluteMonthly`/`absoluteYearly` patterns require `dayOfMonth` (and `month`) outright.
//! So the series' start date is a parameter, not something this module can infer.

use core::num::NonZeroI32;

use engine_core::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday},
    time::CalendarDate,
};
use serde_json::{Value, json};

use crate::error::GraphError;

/// Renders `rule` as a Graph `patternedRecurrence` object, for an event starting on
/// `start`.
///
/// # Errors
///
/// Returns [`GraphError`] if the rule says something Graph's pattern set cannot express —
/// a sub-daily frequency, an ordinal on a weekly rule, a monthly/yearly weekday rule with
/// no ordinal or with mixed ordinals, more than one day-of-month, a day-of-month counted
/// from the end of the month, or any `BYSETPOS`/`BYWEEKNO`/`BYYEARDAY` part.
pub(crate) fn render_recurrence(
    rule: &RecurrenceRule,
    start: CalendarDate,
) -> Result<Value, GraphError> {
    Ok(json!({
        "pattern": render_pattern(rule, start)?,
        "range": render_range(rule, start),
    }))
}

/// The `pattern` half: the frequency, its interval, and the `BY*` parts Graph models as
/// named fields.
fn render_pattern(rule: &RecurrenceRule, start: CalendarDate) -> Result<Value, GraphError> {
    unsupported_parts(rule)?;
    let interval = rule.interval.get();

    let mut pattern = match rule.frequency {
        Frequency::Daily => {
            if !rule.by_day.is_empty() {
                return Err(unsupported(
                    "a daily rule that names weekdays (Graph has no daily BYDAY)",
                ));
            }
            json!({ "type": "daily" })
        }
        Frequency::Weekly => {
            if rule.by_day.iter().any(|d| d.nth_of_period.is_some()) {
                return Err(unsupported(
                    "an ordinal weekday on a weekly rule (Graph's weekly pattern has no index)",
                ));
            }
            // A weekly rule with no BYDAY recurs on DTSTART's weekday; Graph needs it named.
            let days = if rule.by_day.is_empty() {
                vec![weekday_name(weekday_of(start))]
            } else {
                rule.by_day.iter().map(|d| weekday_name(d.day)).collect()
            };
            json!({ "type": "weekly", "daysOfWeek": days })
        }
        Frequency::Monthly => monthly(rule, start)?,
        Frequency::Yearly => yearly(rule, start)?,
        Frequency::Hourly | Frequency::Minutely | Frequency::Secondly => {
            return Err(unsupported(
                "a sub-daily frequency (Graph's smallest pattern is daily)",
            ));
        }
    };

    pattern["interval"] = json!(interval);
    // Graph echoes `firstDayOfWeek` on every pattern but only honours it for weekly rules,
    // which is also the only place the engine's WKST changes an expansion.
    if rule.frequency == Frequency::Weekly {
        pattern["firstDayOfWeek"] = json!(weekday_name(rule.first_day_of_week));
    }
    Ok(pattern)
}

/// `FREQ=MONTHLY` → `absoluteMonthly` (a day of the month) or `relativeMonthly` (an nth
/// weekday).
fn monthly(rule: &RecurrenceRule, start: CalendarDate) -> Result<Value, GraphError> {
    if !rule.by_day.is_empty() {
        return Ok(json!({
            "type": "relativeMonthly",
            "daysOfWeek": rule.by_day.iter().map(|d| weekday_name(d.day)).collect::<Vec<_>>(),
            "index": index_name(shared_ordinal(&rule.by_day)?)?,
        }));
    }
    Ok(json!({ "type": "absoluteMonthly", "dayOfMonth": day_of_month(rule, start)? }))
}

/// `FREQ=YEARLY` → `absoluteYearly` / `relativeYearly`, both of which name a month.
fn yearly(rule: &RecurrenceRule, start: CalendarDate) -> Result<Value, GraphError> {
    let month = match rule.by_month.as_slice() {
        [] => u32::from(start.month()),
        [one] => one.parse::<u32>().map_err(|_| {
            unsupported("a BYMONTH Graph cannot read as a plain month number (RFC 7529 leap month)")
        })?,
        _ => {
            return Err(unsupported(
                "more than one BYMONTH (Graph names a single month)",
            ));
        }
    };
    if !rule.by_day.is_empty() {
        return Ok(json!({
            "type": "relativeYearly",
            "month": month,
            "daysOfWeek": rule.by_day.iter().map(|d| weekday_name(d.day)).collect::<Vec<_>>(),
            "index": index_name(shared_ordinal(&rule.by_day)?)?,
        }));
    }
    Ok(json!({
        "type": "absoluteYearly",
        "month": month,
        "dayOfMonth": day_of_month(rule, start)?,
    }))
}

/// The single positive day-of-month Graph's absolute patterns take, defaulting to the
/// series start's own day.
fn day_of_month(rule: &RecurrenceRule, start: CalendarDate) -> Result<u32, GraphError> {
    match rule.by_month_day.as_slice() {
        [] => Ok(u32::from(start.day())),
        [one] if *one > 0 => {
            u32::try_from(*one).map_err(|_| unsupported("a BYMONTHDAY out of Graph's 1–31 range"))
        }
        [one] if *one < 0 => Err(unsupported(
            "a BYMONTHDAY counted from the end of the month (Graph has no negative dayOfMonth)",
        )),
        _ => Err(unsupported(
            "more than one BYMONTHDAY (Graph names a single dayOfMonth)",
        )),
    }
}

/// The one ordinal every `BYDAY` entry shares — Graph's monthly/yearly patterns carry a
/// single `index` for the whole list, so "first Monday and last Friday" is not expressible.
fn shared_ordinal(days: &[NDay]) -> Result<NonZeroI32, GraphError> {
    let first = days[0].nth_of_period.ok_or_else(|| {
        unsupported("a monthly or yearly weekday rule with no ordinal (Graph needs an index)")
    })?;
    if days.iter().any(|d| d.nth_of_period != Some(first)) {
        return Err(unsupported(
            "BYDAY entries with different ordinals (Graph carries one index for the list)",
        ));
    }
    Ok(first)
}

/// Graph's `index` token for an nth-of-period: 1–4, or `last` for −1.
fn index_name(nth: NonZeroI32) -> Result<&'static str, GraphError> {
    Ok(match nth.get() {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        -1 => "last",
        other => {
            return Err(unsupported(&format!(
                "an ordinal of {other} (Graph indexes only first–fourth and last)"
            )));
        }
    })
}

/// The `range` half: where the series starts, and how it ends.
fn render_range(rule: &RecurrenceRule, start: CalendarDate) -> Value {
    let start_date = iso_date(start);
    match &rule.bound {
        RecurrenceBound::Unbounded => json!({ "type": "noEnd", "startDate": start_date }),
        RecurrenceBound::Count(count) => json!({
            "type": "numbered",
            "startDate": start_date,
            "numberOfOccurrences": count.get(),
        }),
        // Graph's `endDate` is the last *date* an occurrence may start, so the rule's
        // UNTIL instant contributes its date — the inverse of `cal_recur`'s bound parse,
        // which reads an `endDate` back as that day's 23:59:59.
        RecurrenceBound::Until(until) => json!({
            "type": "endDate",
            "startDate": start_date,
            "endDate": format!("{:04}-{:02}-{:02}", until.year(), until.month(), until.day()),
        }),
    }
}

/// The `BY*` parts Graph's pattern set has nowhere to put.
fn unsupported_parts(rule: &RecurrenceRule) -> Result<(), GraphError> {
    for (values, name) in [
        (!rule.by_set_position.is_empty(), "BYSETPOS"),
        (!rule.by_week_no.is_empty(), "BYWEEKNO"),
        (!rule.by_year_day.is_empty(), "BYYEARDAY"),
        (!rule.by_hour.is_empty(), "BYHOUR"),
        (!rule.by_minute.is_empty(), "BYMINUTE"),
        (!rule.by_second.is_empty(), "BYSECOND"),
    ] {
        if values {
            return Err(unsupported(&format!(
                "a {name} part (Graph's patterns have no equivalent)"
            )));
        }
    }
    if let Some(rscale) = &rule.rscale {
        return Err(unsupported(&format!(
            "a non-Gregorian RSCALE={rscale} rule"
        )));
    }
    Ok(())
}

fn unsupported(what: &str) -> GraphError {
    GraphError::protocol(format!("Microsoft Graph cannot express {what}"))
}

/// `YYYY-MM-DD`, the form Graph's `startDate`/`endDate` take.
fn iso_date(date: CalendarDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// The weekday a date falls on, via the days elapsed since a known Monday
/// (1970-01-05). Used only to name `DTSTART`'s own weekday for a `BYDAY`-less weekly rule.
fn weekday_of(date: CalendarDate) -> Weekday {
    // Zeller's congruence over the civil date, avoiding a chrono/jiff dependency in a crate
    // that has neither. January and February count as months 13 and 14 of the previous year.
    let (mut year, mut month) = (i64::from(date.year()), i64::from(date.month()));
    if month < 3 {
        year -= 1;
        month += 12;
    }
    let year_of_century = year % 100;
    let century = year / 100;
    let index = (i64::from(date.day())
        + (13 * (month + 1)) / 5
        + year_of_century
        + year_of_century / 4
        + century / 4
        + 5 * century)
        % 7;
    // Zeller's index: 0 = Saturday, 1 = Sunday, 2 = Monday, …
    match index {
        0 => Weekday::Sa,
        1 => Weekday::Su,
        2 => Weekday::Mo,
        3 => Weekday::Tu,
        4 => Weekday::We,
        5 => Weekday::Th,
        _ => Weekday::Fr,
    }
}

/// Graph's full lower-case weekday names.
fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mo => "monday",
        Weekday::Tu => "tuesday",
        Weekday::We => "wednesday",
        Weekday::Th => "thursday",
        Weekday::Fr => "friday",
        Weekday::Sa => "saturday",
        Weekday::Su => "sunday",
    }
}

#[cfg(test)]
#[path = "cal_recur_render_tests.rs"]
mod tests;
