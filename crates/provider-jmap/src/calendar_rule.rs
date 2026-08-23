//! The engine's [`RecurrenceRule`] → a JSCalendar `RecurrenceRule` object (RFC 8984 §4.3.3).
//!
//! JMAP is the one transport that takes recurrence in the *same shape* the engine models it
//! — a structured rule rather than an `RRULE` string (CalDAV, Google) or a named pattern
//! (Graph). It still cannot be `serde_json::to_value(rule)`, for two reasons that would each
//! produce an object the server rejects: the engine's field names are Rust's
//! (`by_day`, `first_day_of_week`), while JSCalendar's are camelCase; and the engine folds
//! termination into one `bound` enum, while JSCalendar has two mutually exclusive
//! properties, `count` and `until`.
//!
//! # `until` is local here, and that is not an inconsistency
//!
//! RFC 8984 §4.3.3 defines `until` as a `LocalDateTime` **in the event's own time zone** —
//! exactly what [`RecurrenceBound::Until`] holds. So unlike CalDAV and Google, this adapter
//! needs no resolved instant and ignores `DraftRecurrence::until`. iCalendar is the odd one
//! out (RFC 5545 §3.3.10 demands UTC), which is why that field exists at all.

use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
use serde_json::{Map, Value, json};

use crate::error::JmapError;

/// Renders `rule` as a JSCalendar `RecurrenceRule` object.
///
/// Defaults are omitted (`interval` 1, `firstDayOfWeek` `mo`, `skip` `omit`) so the object
/// carries only what the rule actually says.
///
/// # Errors
///
/// Returns [`JmapError`] if the rule is non-Gregorian (RFC 7529 `RSCALE`). JSCalendar can
/// carry `rscale`, but the engine never *expands* such a rule (`calendar-semantics.md`), so
/// writing one would store a series this engine would then draw as empty — refused here
/// rather than shipped.
pub(crate) fn render_rule(rule: &RecurrenceRule) -> Result<Value, JmapError> {
    if let Some(rscale) = &rule.rscale {
        return Err(JmapError::protocol(format!(
            "cannot write a non-Gregorian RSCALE={rscale} recurrence: the engine preserves \
             such a rule but never expands it, so the series would store and draw nothing"
        )));
    }

    let mut out = Map::new();
    out.insert("@type".to_owned(), json!("RecurrenceRule"));
    out.insert("frequency".to_owned(), json!(frequency(rule.frequency)));

    if rule.interval.get() != 1 {
        out.insert("interval".to_owned(), json!(rule.interval.get()));
    }
    if rule.first_day_of_week != Weekday::Mo {
        out.insert(
            "firstDayOfWeek".to_owned(),
            json!(weekday(rule.first_day_of_week)),
        );
    }
    if !rule.by_day.is_empty() {
        out.insert(
            "byDay".to_owned(),
            Value::Array(rule.by_day.iter().copied().map(nday).collect()),
        );
    }
    insert_if_any(&mut out, "byMonthDay", &rule.by_month_day);
    if !rule.by_month.is_empty() {
        out.insert("byMonth".to_owned(), json!(rule.by_month));
    }
    insert_if_any(&mut out, "byYearDay", &rule.by_year_day);
    insert_if_any(&mut out, "byWeekNo", &rule.by_week_no);
    insert_if_any(&mut out, "byHour", &rule.by_hour);
    insert_if_any(&mut out, "byMinute", &rule.by_minute);
    insert_if_any(&mut out, "bySecond", &rule.by_second);
    insert_if_any(&mut out, "bySetPosition", &rule.by_set_position);

    // `count` and `until` are mutually exclusive in JSCalendar, which the engine's single
    // `bound` already makes unrepresentable to get wrong.
    match &rule.bound {
        RecurrenceBound::Unbounded => {}
        RecurrenceBound::Count(count) => {
            out.insert("count".to_owned(), json!(count.get()));
        }
        RecurrenceBound::Until(local) => {
            out.insert("until".to_owned(), json!(local.to_string()));
        }
    }

    Ok(Value::Object(out))
}

/// A `byDay` entry: the weekday, plus its ordinal within the period when it has one.
fn nday(day: NDay) -> Value {
    let mut entry = Map::new();
    entry.insert("@type".to_owned(), json!("NDay"));
    entry.insert("day".to_owned(), json!(weekday(day.day)));
    if let Some(nth) = day.nth_of_period {
        entry.insert("nthOfPeriod".to_owned(), json!(nth.get()));
    }
    Value::Object(entry)
}

/// Inserts a `by*` array only when it has entries — an empty one means "absent".
fn insert_if_any<T: Copy + Into<Value>>(out: &mut Map<String, Value>, key: &str, values: &[T]) {
    if !values.is_empty() {
        out.insert(
            key.to_owned(),
            Value::Array(values.iter().map(|v| (*v).into()).collect()),
        );
    }
}

/// The JSCalendar `frequency` token.
fn frequency(frequency: Frequency) -> &'static str {
    match frequency {
        Frequency::Yearly => "yearly",
        Frequency::Monthly => "monthly",
        Frequency::Weekly => "weekly",
        Frequency::Daily => "daily",
        Frequency::Hourly => "hourly",
        Frequency::Minutely => "minutely",
        Frequency::Secondly => "secondly",
    }
}

/// The JSCalendar two-letter weekday token.
fn weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Mo => "mo",
        Weekday::Tu => "tu",
        Weekday::We => "we",
        Weekday::Th => "th",
        Weekday::Fr => "fr",
        Weekday::Sa => "sa",
        Weekday::Su => "su",
    }
}

#[cfg(test)]
#[path = "calendar_rule_tests.rs"]
mod tests;
