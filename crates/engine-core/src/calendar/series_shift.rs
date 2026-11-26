//! Where the occurrences of a series go when the series' own start moves.
//!
//! An occurrence is named by the slot the rule gave it: its **original** start, the key of a
//! `RECURRENCE-ID` override and an `EXDATE` (RFC 5545 §3.8.4.4, §3.8.5.1) and of a
//! `recurrenceOverrides` entry (RFC 8984 §4.3.3). Move the series and the rule gives other
//! slots, so an override still keyed by the old one names no occurrence. It stops standing in
//! for the occurrence it was made for, which comes back, and the expander draws the orphan as an
//! added occurrence beside it: one occurrence on screen twice. An exclusion left behind
//! excludes nothing, and the occurrence the user deleted returns. A transport that keeps its
//! overrides through a series move therefore moves their keys with it, by this.
//!
//! The keys move by what the move did to the **slots**, which is not always what it did to the
//! start. A rule that names its days (`BYDAY`, `BYMONTHDAY`, `BYYEARDAY`, `BYWEEKNO`) keeps
//! them however far the start moves, so its slots move only in the time of day; one that names
//! its hours, minutes or seconds keeps those, and its slots move only in the day.

use super::RecurrenceRule;
use crate::time::{CalendarDate, CalendarDateTime, LocalDateTime};

/// How moving a series' start moves the slots its occurrences are named by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesShift {
    days: i64,
    seconds: i64,
}

impl SeriesShift {
    /// The shift that moving a series from `from` to `to` gives the slots `rules` produce, or
    /// `None` when it moves none of them: the start did not move, its form changed (which no
    /// write allows), or the rules name exactly what moved.
    #[must_use]
    pub fn between(
        from: &CalendarDateTime,
        to: &CalendarDateTime,
        rules: &[RecurrenceRule],
    ) -> Option<Self> {
        if !from.has_same_form(to) {
            return None;
        }
        let (days, seconds) = match (from, to) {
            (CalendarDateTime::Date(a), CalendarDateTime::Date(b)) => (day_span(*a, *b), 0),
            _ => {
                let (a, b) = (from.local()?.as_primitive(), to.local()?.as_primitive());
                (
                    i64::from(b.date().to_julian_day()) - i64::from(a.date().to_julian_day()),
                    (b.time() - a.time()).whole_seconds(),
                )
            }
        };
        let days = if rules.iter().any(names_its_days) {
            0
        } else {
            days
        };
        let seconds = if rules.iter().any(names_its_times) {
            0
        } else {
            seconds
        };
        (days != 0 || seconds != 0).then_some(Self { days, seconds })
    }

    /// Where the occurrence that was at `slot` is now, or `None` outside representable time.
    #[must_use]
    pub fn slot(self, slot: LocalDateTime) -> Option<LocalDateTime> {
        slot.as_primitive()
            .checked_add(time::Duration::days(self.days))?
            .checked_add(time::Duration::seconds(self.seconds))
            .map(LocalDateTime::from_primitive)
    }

    /// Where a slot written as `value` is now, in the form it was written in.
    ///
    /// A date names a day and has no time to move, so it moves by days alone: an all-day
    /// series, or a `DATE`-valued `EXDATE` on a timed one, which the reader resolves against the
    /// series' own time of day.
    #[must_use]
    pub fn value(self, value: &CalendarDateTime) -> Option<CalendarDateTime> {
        match value {
            CalendarDateTime::Date(date) => {
                date.checked_add_days(self.days).map(CalendarDateTime::Date)
            }
            CalendarDateTime::Floating(local) => self.slot(*local).map(CalendarDateTime::Floating),
            CalendarDateTime::Zoned { local, zone } => {
                self.slot(*local).map(|local| CalendarDateTime::Zoned {
                    local,
                    zone: zone.clone(),
                })
            }
        }
    }
}

fn day_span(from: CalendarDate, to: CalendarDate) -> i64 {
    i64::from(to.as_date().to_julian_day()) - i64::from(from.as_date().to_julian_day())
}

/// Whether `rule` fixes the days it falls on, rather than taking them from the start.
fn names_its_days(rule: &RecurrenceRule) -> bool {
    !rule.by_day.is_empty()
        || !rule.by_month_day.is_empty()
        || !rule.by_year_day.is_empty()
        || !rule.by_week_no.is_empty()
}

/// Whether `rule` fixes the times it falls at, rather than taking them from the start.
fn names_its_times(rule: &RecurrenceRule) -> bool {
    !rule.by_hour.is_empty() || !rule.by_minute.is_empty() || !rule.by_second.is_empty()
}
