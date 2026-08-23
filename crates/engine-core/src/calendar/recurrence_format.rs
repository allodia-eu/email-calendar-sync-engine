//! Rendering a [`RecurrenceRule`] back to an RFC 5545 `RRULE` **value** string.
//!
//! The inverse of [`parse_rrule`](super::parse_rrule), and the shared renderer for every
//! provider whose wire format carries a raw `RRULE`: iCalendar/CalDAV's `RRULE:` property
//! and Google Calendar's `recurrence` array. A provider that takes a *structured*
//! recurrence (Microsoft Graph's `patternedRecurrence`) renders it from the same
//! [`RecurrenceRule`] without coming through here.
//!
//! # `UNTIL` is the caller's to resolve
//!
//! [`RecurrenceBound::Until`] holds a **wall clock in the event's own zone**, but RFC 5545
//! §3.3.10 ties the rendered form to the series' `DTSTART`: a zoned or UTC `DTSTART`
//! requires `UNTIL` **in UTC**. Resolving a wall clock through a zone needs tzdata, which
//! this crate deliberately does not have (it lives in `engine-recurrence`), so the caller
//! states the form through [`UntilForm`] and supplies the resolved instant for the zoned
//! case. That is what makes "I forgot to convert to UTC" unrepresentable rather than a
//! silent bug that ends a series on the wrong day for readers in another zone.
//!
//! # Round-tripping
//!
//! `parse_rrule(format_rrule(rule, …)?)` returns `rule` for every rule this renderer
//! accepts. The reverse does **not** hold, and is not meant to: the parser normalizes
//! (a date-only `UNTIL=20261231` becomes `23:59:59`, an absent `INTERVAL` becomes 1), so
//! rendering a parsed rule produces the canonical spelling rather than the original bytes.
//! Preserving the original bytes is the raw payload's job, never this one's.

use core::fmt::Write as _;

use super::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
use crate::time::{LocalDateTime, UtcDateTime};

/// A failure rendering a [`RecurrenceRule`] as an `RRULE` value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RruleFormatError {
    /// The rule is non-Gregorian (RFC 7529 `RSCALE`), which this renderer will not
    /// silently drop.
    ///
    /// Emitting the rule without its `RSCALE` would change what it means — a Hebrew-calendar
    /// yearly rule would become a Gregorian one — and the engine's contract for non-Gregorian
    /// recurrence is that it is *preserved raw, never expanded* (`calendar-semantics.md`). So
    /// a caller that meets this writes the preserved payload back instead of a rendered rule.
    #[error("cannot render a non-Gregorian RSCALE={0:?} rule as an RRULE without losing it")]
    NonGregorian(String),
}

/// How `UNTIL` is rendered, which RFC 5545 §3.3.10 ties to the series' `DTSTART` form.
///
/// Only consulted when the rule's [`bound`](RecurrenceRule::bound) is
/// [`RecurrenceBound::Until`]; an unbounded or counted rule ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UntilForm {
    /// `DTSTART` is a DATE — an all-day series. `UNTIL` renders as `YYYYMMDD`, dropping the
    /// wall clock's time of day.
    Date,
    /// `DTSTART` is a floating date-time. `UNTIL` renders as `YYYYMMDDTHHMMSS`, in the same
    /// floating terms.
    Floating,
    /// `DTSTART` is UTC or carries a `TZID`. `UNTIL` **must** be UTC (`YYYYMMDDTHHMMSSZ`),
    /// so the caller resolves the rule's wall clock through the event's zone and supplies
    /// the instant here.
    Utc(UtcDateTime),
}

/// Renders `rule` as an RFC 5545 `RRULE` value — the text that follows `RRULE:`.
///
/// Parts are emitted in a stable order (`FREQ` first, `WKST` last) so the same rule always
/// produces the same bytes, and defaults are omitted (`INTERVAL=1`, `WKST=MO`) so the
/// output matches what servers echo back.
///
/// # Errors
///
/// Returns [`RruleFormatError::NonGregorian`] if the rule carries an `RSCALE`, which this
/// renderer refuses rather than silently dropping.
pub fn format_rrule(rule: &RecurrenceRule, until: UntilForm) -> Result<String, RruleFormatError> {
    if let Some(rscale) = &rule.rscale {
        return Err(RruleFormatError::NonGregorian(rscale.clone()));
    }

    let mut out = String::from("FREQ=");
    out.push_str(frequency_token(rule.frequency));

    if rule.interval.get() != 1 {
        let _ = write!(out, ";INTERVAL={}", rule.interval);
    }

    match &rule.bound {
        RecurrenceBound::Unbounded => {}
        RecurrenceBound::Count(count) => {
            let _ = write!(out, ";COUNT={count}");
        }
        RecurrenceBound::Until(local) => {
            let _ = write!(out, ";UNTIL={}", render_until(*local, until));
        }
    }

    // RFC 5545 lists the BY* parts smallest-component-first; keeping that order makes the
    // rendered rule comparable with what most servers emit.
    push_uints(&mut out, "BYSECOND", &rule.by_second);
    push_uints(&mut out, "BYMINUTE", &rule.by_minute);
    push_uints(&mut out, "BYHOUR", &rule.by_hour);
    push_by_day(&mut out, &rule.by_day);
    push_ints(&mut out, "BYMONTHDAY", &rule.by_month_day);
    push_ints(&mut out, "BYYEARDAY", &rule.by_year_day);
    push_ints(&mut out, "BYWEEKNO", &rule.by_week_no);
    push_strs(&mut out, "BYMONTH", &rule.by_month);
    push_ints(&mut out, "BYSETPOS", &rule.by_set_position);

    if rule.first_day_of_week != Weekday::Mo {
        let _ = write!(out, ";WKST={}", weekday_token(rule.first_day_of_week));
    }

    Ok(out)
}

/// Renders the `UNTIL` value in the form the series' `DTSTART` requires.
fn render_until(local: LocalDateTime, form: UntilForm) -> String {
    match form {
        UntilForm::Date => basic_date(local.year(), local.month(), local.day()),
        UntilForm::Floating => basic_date_time(
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.second(),
        ),
        UntilForm::Utc(at) => format!(
            "{}Z",
            basic_date_time(
                at.year(),
                at.month(),
                at.day(),
                at.hour(),
                at.minute(),
                at.second(),
            )
        ),
    }
}

/// An RFC 5545 basic-format date: `YYYYMMDD`.
fn basic_date(year: i32, month: u8, day: u8) -> String {
    format!("{year:04}{month:02}{day:02}")
}

/// An RFC 5545 basic-format date-time: `YYYYMMDDTHHMMSS`.
fn basic_date_time(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> String {
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}")
}

/// `BYDAY`, each entry an optional signed ordinal followed by the two-letter weekday.
fn push_by_day(out: &mut String, days: &[NDay]) {
    if days.is_empty() {
        return;
    }
    out.push_str(";BYDAY=");
    for (i, nday) in days.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if let Some(nth) = nday.nth_of_period {
            let _ = write!(out, "{nth}");
        }
        out.push_str(weekday_token(nday.day));
    }
}

/// A `;KEY=v1,v2` part, skipped entirely when the list is empty.
fn push_ints(out: &mut String, key: &str, values: &[i32]) {
    push_list(out, key, values.iter().map(i32::to_string));
}

fn push_uints(out: &mut String, key: &str, values: &[u8]) {
    push_list(out, key, values.iter().map(u8::to_string));
}

fn push_strs(out: &mut String, key: &str, values: &[String]) {
    push_list(out, key, values.iter().cloned());
}

fn push_list(out: &mut String, key: &str, values: impl Iterator<Item = String>) {
    let joined = values.collect::<Vec<_>>().join(",");
    if !joined.is_empty() {
        let _ = write!(out, ";{key}={joined}");
    }
}

/// The iCalendar `FREQ` token for a [`Frequency`].
fn frequency_token(frequency: Frequency) -> &'static str {
    match frequency {
        Frequency::Yearly => "YEARLY",
        Frequency::Monthly => "MONTHLY",
        Frequency::Weekly => "WEEKLY",
        Frequency::Daily => "DAILY",
        Frequency::Hourly => "HOURLY",
        Frequency::Minutely => "MINUTELY",
        Frequency::Secondly => "SECONDLY",
    }
}

/// The two-letter iCalendar token for a [`Weekday`].
fn weekday_token(day: Weekday) -> &'static str {
    match day {
        Weekday::Mo => "MO",
        Weekday::Tu => "TU",
        Weekday::We => "WE",
        Weekday::Th => "TH",
        Weekday::Fr => "FR",
        Weekday::Sa => "SA",
        Weekday::Su => "SU",
    }
}

#[cfg(test)]
#[path = "recurrence_format_tests.rs"]
mod tests;
