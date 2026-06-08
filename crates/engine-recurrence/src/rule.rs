//! Pure RRULE date generation over the supported subset.
//!
//! Generates the ordered, de-duplicated set of **base recurrence-id dates** for one
//! [`RecurrenceRule`], working entirely in civil (wall-clock) dates — the
//! time-of-day is fixed by the master start, and zone resolution happens later in
//! [`crate::zone`]. The supported subset and what is rejected are documented on the
//! crate. Generation is ascending and stops as soon as `COUNT` is reached or the
//! horizon-derived `window_end` is passed, so a bounded series never costs more
//! than its length.

use core::cmp::Ordering;
use std::collections::BTreeSet;

use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};
use jiff::Span;
use jiff::civil::{Date, Weekday as JWeekday};

use crate::ExpandError;

/// Builds a civil date, mapping an out-of-range value to [`ExpandError`].
fn ymd(year: i16, month: i8, day: i8) -> Result<Date, ExpandError> {
    Date::new(year, month, day).map_err(|_| ExpandError::OutOfRange)
}

/// Adds `days` to a date.
fn add_days(date: Date, days: i64) -> Result<Date, ExpandError> {
    date.checked_add(Span::new().days(days))
        .map_err(|_| ExpandError::OutOfRange)
}

/// Maps an engine weekday to jiff's.
fn jiff_weekday(weekday: Weekday) -> JWeekday {
    match weekday {
        Weekday::Mo => JWeekday::Monday,
        Weekday::Tu => JWeekday::Tuesday,
        Weekday::We => JWeekday::Wednesday,
        Weekday::Th => JWeekday::Thursday,
        Weekday::Fr => JWeekday::Friday,
        Weekday::Sa => JWeekday::Saturday,
        Weekday::Su => JWeekday::Sunday,
    }
}

/// Advances `(year, month)` by `add` months, normalizing the month into 1..=12.
fn add_months(year: i16, month: i8, add: u32) -> Option<(i16, i8)> {
    let zero_based = i32::from(month) - 1 + i32::try_from(add).ok()?;
    let new_year = i32::from(year) + zero_based.div_euclid(12);
    let new_month = zero_based.rem_euclid(12) + 1;
    Some((i16::try_from(new_year).ok()?, i8::try_from(new_month).ok()?))
}

/// Parses `BYMONTH` ("1".."12", optional `L` leap suffix ignored) into a month set.
fn parse_by_month(by_month: &[String]) -> Result<Option<BTreeSet<i8>>, ExpandError> {
    if by_month.is_empty() {
        return Ok(None);
    }
    let mut set = BTreeSet::new();
    for entry in by_month {
        let digits: String = entry.chars().take_while(char::is_ascii_digit).collect();
        let month: i8 = digits
            .parse()
            .map_err(|_| ExpandError::UnsupportedRule("malformed BYMONTH"))?;
        if !(1..=12).contains(&month) {
            return Err(ExpandError::OutOfRange);
        }
        set.insert(month);
    }
    Ok(Some(set))
}

/// Rejects rule parts outside the supported subset (see the crate docs).
fn check_supported(rule: &RecurrenceRule) -> Result<(), ExpandError> {
    if rule.rscale.is_some() {
        return Err(ExpandError::UnsupportedRule(
            "RSCALE / non-Gregorian recurrence",
        ));
    }
    if !matches!(
        rule.frequency,
        Frequency::Daily | Frequency::Weekly | Frequency::Monthly | Frequency::Yearly
    ) {
        return Err(ExpandError::UnsupportedRule("sub-daily frequency"));
    }
    if !rule.by_year_day.is_empty() {
        return Err(ExpandError::UnsupportedRule("BYYEARDAY"));
    }
    if !rule.by_week_no.is_empty() {
        return Err(ExpandError::UnsupportedRule("BYWEEKNO"));
    }
    if !rule.by_set_position.is_empty() {
        return Err(ExpandError::UnsupportedRule("BYSETPOS"));
    }
    if !rule.by_hour.is_empty() || !rule.by_minute.is_empty() || !rule.by_second.is_empty() {
        return Err(ExpandError::UnsupportedRule("BYHOUR/BYMINUTE/BYSECOND"));
    }
    let has_nth = rule.by_day.iter().any(|nd| nd.nth_of_period.is_some());
    if has_nth && matches!(rule.frequency, Frequency::Daily | Frequency::Weekly) {
        return Err(ExpandError::UnsupportedRule(
            "nth BYDAY requires MONTHLY or YEARLY",
        ));
    }
    if has_nth && rule.frequency == Frequency::Yearly && rule.by_month.is_empty() {
        return Err(ExpandError::UnsupportedRule(
            "year-relative nth BYDAY (without BYMONTH)",
        ));
    }
    Ok(())
}

/// Accumulates the generated dates with bounds, de-duplication, and the count cap.
struct Acc {
    out: Vec<Date>,
    seen: BTreeSet<Date>,
    start: Date,
    upper: Date,
    by_month: Option<BTreeSet<i8>>,
    count: Option<usize>,
    cap: usize,
}

impl Acc {
    /// Records `date` if it is in range, passes the `BYMONTH` limit, and is new.
    /// Returns `true` when generation should stop because `COUNT` is satisfied.
    fn emit(&mut self, date: Date) -> Result<bool, ExpandError> {
        if date < self.start || date > self.upper {
            return Ok(false);
        }
        if let Some(set) = &self.by_month
            && !set.contains(&date.month())
        {
            return Ok(false);
        }
        if self.seen.insert(date) {
            self.out.push(date);
            if self.out.len() > self.cap {
                return Err(ExpandError::TooManyInstances(self.cap));
            }
        }
        Ok(self.count.is_some_and(|c| self.out.len() >= c))
    }
}

/// Generates the ordered, unique base dates for one rule within
/// `[start, window_end]`, honoring `COUNT`/`UNTIL` and the safety `cap`.
///
/// The returned dates are recurrence-id dates (the rule output before overrides and
/// exclusions); the caller applies the time-of-day, overrides, and precise horizon
/// and `UNTIL` filtering.
///
/// # Errors
///
/// Returns [`ExpandError::UnsupportedRule`] for an out-of-subset rule,
/// [`ExpandError::TooManyInstances`] past the cap, or [`ExpandError::OutOfRange`]
/// for an unrepresentable date.
pub(crate) fn occurrence_dates(
    start: Date,
    rule: &RecurrenceRule,
    window_end: Date,
    cap: usize,
) -> Result<Vec<Date>, ExpandError> {
    check_supported(rule)?;
    let (count, until_date) = match &rule.bound {
        RecurrenceBound::Unbounded => (None, None),
        RecurrenceBound::Count(n) => (Some(usize::try_from(n.get()).unwrap_or(usize::MAX)), None),
        RecurrenceBound::Until(until) => (
            None,
            Some(ymd(
                i16::try_from(until.year()).map_err(|_| ExpandError::OutOfRange)?,
                i8::try_from(until.month()).map_err(|_| ExpandError::OutOfRange)?,
                i8::try_from(until.day()).map_err(|_| ExpandError::OutOfRange)?,
            )?),
        ),
    };
    let upper = match until_date {
        Some(until) => until.min(window_end),
        None => window_end,
    };
    let mut acc = Acc {
        out: Vec::new(),
        seen: BTreeSet::new(),
        start,
        upper,
        by_month: parse_by_month(&rule.by_month)?,
        count,
        cap,
    };
    // RFC 5545: DTSTART is always the first instance of the set.
    if !acc.emit(start)? {
        let interval = rule.interval.get();
        match rule.frequency {
            Frequency::Daily => generate_daily(&mut acc, interval, rule)?,
            Frequency::Weekly => generate_weekly(&mut acc, interval, rule)?,
            Frequency::Monthly => generate_monthly(&mut acc, interval, rule)?,
            Frequency::Yearly => generate_yearly(&mut acc, interval, rule)?,
            // Sub-daily was rejected by `check_supported`.
            _ => unreachable!("frequency checked by check_supported"),
        }
    }
    // `seen` gates every push, so `out` is already duplicate-free; sort to a
    // defined ascending order so `COUNT` truncation keeps the earliest instances.
    let mut out = acc.out;
    out.sort_unstable();
    if let Some(limit) = count {
        out.truncate(limit);
    }
    Ok(out)
}

/// `DAILY`: every `interval` days from the start, filtered by `BYDAY`/`BYMONTHDAY`.
fn generate_daily(acc: &mut Acc, interval: u32, rule: &RecurrenceRule) -> Result<(), ExpandError> {
    let step = i64::from(interval);
    let mut date = acc.start;
    loop {
        date = add_days(date, step)?;
        if date > acc.upper {
            return Ok(());
        }
        if daily_matches(date, rule) && acc.emit(date)? {
            return Ok(());
        }
    }
}

/// Whether a daily candidate passes the `BYDAY` and `BYMONTHDAY` filters.
fn daily_matches(date: Date, rule: &RecurrenceRule) -> bool {
    if !rule.by_day.is_empty()
        && !rule
            .by_day
            .iter()
            .any(|nd| jiff_weekday(nd.day) == date.weekday())
    {
        return false;
    }
    if !rule.by_month_day.is_empty() {
        let dim = i32::from(date.days_in_month());
        let day = i32::from(date.day());
        if !rule
            .by_month_day
            .iter()
            .any(|&md| month_day(md, dim) == day)
        {
            return false;
        }
    }
    true
}

/// `WEEKLY`: every `interval` weeks, the `BYDAY` weekdays (or the start's), aligned
/// to `WKST`.
fn generate_weekly(acc: &mut Acc, interval: u32, rule: &RecurrenceRule) -> Result<(), ExpandError> {
    let wkst = i64::from(jiff_weekday(rule.first_day_of_week).to_monday_zero_offset());
    let start_off = i64::from(acc.start.weekday().to_monday_zero_offset());
    let mut anchor = add_days(acc.start, -((start_off - wkst).rem_euclid(7)))?;
    let step = i64::from(interval) * 7;
    let targets: Vec<JWeekday> = if rule.by_day.is_empty() {
        vec![acc.start.weekday()]
    } else {
        rule.by_day.iter().map(|nd| jiff_weekday(nd.day)).collect()
    };
    loop {
        let mut week: Vec<Date> = Vec::with_capacity(targets.len());
        for &weekday in &targets {
            let offset = (i64::from(weekday.to_monday_zero_offset()) - wkst).rem_euclid(7);
            week.push(add_days(anchor, offset)?);
        }
        week.sort_unstable();
        week.dedup();
        for date in week {
            if date > acc.upper {
                return Ok(());
            }
            if acc.emit(date)? {
                return Ok(());
            }
        }
        anchor = add_days(anchor, step)?;
        if anchor > acc.upper {
            return Ok(());
        }
    }
}

/// `MONTHLY`: every `interval` months, the days selected by `BYMONTHDAY`/`BYDAY`.
fn generate_monthly(
    acc: &mut Acc,
    interval: u32,
    rule: &RecurrenceRule,
) -> Result<(), ExpandError> {
    let (mut year, mut month) = (acc.start.year(), acc.start.month());
    loop {
        if ymd(year, month, 1)? > acc.upper {
            return Ok(());
        }
        for date in days_in_month(year, month, rule, acc.start)? {
            if acc.emit(date)? {
                return Ok(());
            }
        }
        let (next_year, next_month) =
            add_months(year, month, interval).ok_or(ExpandError::OutOfRange)?;
        year = next_year;
        month = next_month;
    }
}

/// `YEARLY`: every `interval` years, over the `BYMONTH` months (or the start's),
/// with the days selected per month.
fn generate_yearly(acc: &mut Acc, interval: u32, rule: &RecurrenceRule) -> Result<(), ExpandError> {
    // `occurrence_dates` already parsed `BYMONTH` into `acc.by_month`; reuse it.
    let months: Vec<i8> = match &acc.by_month {
        Some(set) => set.iter().copied().collect(),
        None => vec![acc.start.month()],
    };
    let step = i16::try_from(interval).map_err(|_| ExpandError::OutOfRange)?;
    let mut year = acc.start.year();
    loop {
        if ymd(year, 1, 1)? > acc.upper {
            return Ok(());
        }
        for &month in &months {
            for date in days_in_month(year, month, rule, acc.start)? {
                if acc.emit(date)? {
                    return Ok(());
                }
            }
        }
        year = year.checked_add(step).ok_or(ExpandError::OutOfRange)?;
    }
}

/// The sorted candidate days for one `(year, month)` per `BYMONTHDAY`/`BYDAY`, or
/// the start's day-of-month when neither is set.
fn days_in_month(
    year: i16,
    month: i8,
    rule: &RecurrenceRule,
    start: Date,
) -> Result<Vec<Date>, ExpandError> {
    let by_md = !rule.by_month_day.is_empty();
    let by_day = !rule.by_day.is_empty();
    let mut days = match (by_md, by_day) {
        (true, true) => {
            let weekdays: BTreeSet<Date> = weekday_dates(year, month, &rule.by_day)?
                .into_iter()
                .collect();
            month_day_dates(year, month, &rule.by_month_day)?
                .into_iter()
                .filter(|d| weekdays.contains(d))
                .collect()
        }
        (true, false) => month_day_dates(year, month, &rule.by_month_day)?,
        (false, true) => weekday_dates(year, month, &rule.by_day)?,
        (false, false) => {
            let first = ymd(year, month, 1)?;
            let dim = i32::from(first.days_in_month());
            let day = i32::from(start.day());
            if day <= dim {
                vec![ymd(
                    year,
                    month,
                    i8::try_from(day).map_err(|_| ExpandError::OutOfRange)?,
                )?]
            } else {
                Vec::new()
            }
        }
    };
    days.sort_unstable();
    days.dedup();
    Ok(days)
}

/// Resolves `BYMONTHDAY` entries (including negatives) to the valid days of the
/// month.
fn month_day_dates(year: i16, month: i8, by_month_day: &[i32]) -> Result<Vec<Date>, ExpandError> {
    let dim = i32::from(ymd(year, month, 1)?.days_in_month());
    let mut dates = Vec::new();
    for &md in by_month_day {
        let day = month_day(md, dim);
        if (1..=dim).contains(&day) {
            dates.push(ymd(
                year,
                month,
                i8::try_from(day).map_err(|_| ExpandError::OutOfRange)?,
            )?);
        }
    }
    Ok(dates)
}

/// Resolves a `BYMONTHDAY` value against a month length: positive is itself,
/// negative counts from the end, zero is invalid (returns an out-of-range 0).
fn month_day(md: i32, days_in_month: i32) -> i32 {
    match md.cmp(&0) {
        Ordering::Greater => md,
        Ordering::Less => days_in_month + md + 1,
        Ordering::Equal => 0,
    }
}

/// Resolves `BYDAY` entries for one `(year, month)`: an nth-of-month occurrence
/// when `nth` is set, otherwise every matching weekday in the month.
fn weekday_dates(year: i16, month: i8, by_day: &[NDay]) -> Result<Vec<Date>, ExpandError> {
    let first = ymd(year, month, 1)?;
    let dim = first.days_in_month();
    let mut dates = Vec::new();
    for nd in by_day {
        let weekday = jiff_weekday(nd.day);
        match nd.nth_of_period {
            Some(nth) => {
                let nth = i8::try_from(nth.get()).map_err(|_| ExpandError::OutOfRange)?;
                if let Ok(date) = first.nth_weekday_of_month(nth, weekday) {
                    dates.push(date);
                }
            }
            None => {
                for day in 1..=dim {
                    let date = ymd(year, month, day)?;
                    if date.weekday() == weekday {
                        dates.push(date);
                    }
                }
            }
        }
    }
    Ok(dates)
}
