//! The four-case scheduled-time value.

use serde::{Deserialize, Serialize};

use super::{CalendarDate, Duration, LocalDateTime, TimeError, TimeZoneId};

/// Converts a `time` span into a calendar [`Duration`], splitting it into nominal
/// whole days plus the absolute remainder (so a span survives DST per
/// [`Duration`]'s day/second model). A negative span is rejected.
fn span_to_duration(span: time::Duration) -> Result<Duration, TimeError> {
    if span.is_negative() {
        return Err(TimeError::OutOfRange);
    }
    let days = span.whole_days();
    let rest = span - time::Duration::days(days);
    Duration::from_parts(
        0,
        u64::try_from(days).map_err(|_| TimeError::OutOfRange)?,
        0,
        0,
        u64::try_from(rest.whole_seconds()).map_err(|_| TimeError::OutOfRange)?,
        u32::try_from(rest.subsec_nanoseconds()).map_err(|_| TimeError::OutOfRange)?,
    )
}

/// A scheduled time as the engine stores it.
///
/// Every event start, recurrence id, and `until` bound is one of these. Note the
/// deliberate absence of a "UTC instant" variant for scheduled times: UTC is
/// represented as [`CalendarDateTime::Zoned`] with the zone `Etc/UTC`
/// ([`CalendarDateTime::utc`]), matching JSCalendar, which never stores an
/// event start in UTC (RFC 8984 §1.1). True UTC instants — `created`, `updated`,
/// `DTSTAMP` — use [`super::UtcDateTime`] instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum CalendarDateTime {
    /// A zoneless calendar date: an all-day or multi-day value. Has no zone and
    /// no DST, so it denotes the same date in every zone (RFC 5545 `DATE`;
    /// JSCalendar `showWithoutTime`).
    Date(CalendarDate),
    /// A floating wall-clock time with no zone (RFC 5545 form #1; JSCalendar
    /// `timeZone == null`). It occurs at the given wall-clock time in *each*
    /// zone, so its membership in a time range can shift with the observer's
    /// zone — inherent to floating time, not a defect.
    Floating(LocalDateTime),
    /// A zoned wall-clock time, resolved to an instant through `zone` (RFC 5545
    /// `TZID`; JSCalendar `timeZone`). UTC is this variant with `Etc/UTC`.
    Zoned {
        /// The wall-clock value.
        local: LocalDateTime,
        /// The zone that resolves `local` to an instant.
        zone: TimeZoneId,
    },
}

impl CalendarDateTime {
    /// Creates a UTC scheduled time (a zoned value in `Etc/UTC`).
    #[must_use]
    pub fn utc(local: LocalDateTime) -> Self {
        Self::Zoned {
            local,
            zone: TimeZoneId::utc(),
        }
    }

    /// Returns `true` if this is an all-day / date-only value.
    #[must_use]
    pub fn is_all_day(&self) -> bool {
        matches!(self, Self::Date(_))
    }

    /// Returns `true` if this is a floating (zoneless) wall-clock time.
    #[must_use]
    pub fn is_floating(&self) -> bool {
        matches!(self, Self::Floating(_))
    }

    /// Returns the zone, if this value carries one. All-day and floating values
    /// return `None`; attaching a zone to them would be incorrect.
    #[must_use]
    pub fn zone(&self) -> Option<&TimeZoneId> {
        match self {
            Self::Zoned { zone, .. } => Some(zone),
            Self::Date(_) | Self::Floating(_) => None,
        }
    }

    /// Returns the wall-clock value, if this is a floating or zoned time.
    #[must_use]
    pub fn local(&self) -> Option<LocalDateTime> {
        match self {
            Self::Floating(local) | Self::Zoned { local, .. } => Some(*local),
            Self::Date(_) => None,
        }
    }

    /// The [`Duration`] from this start to `end`, the way iCalendar derives an
    /// event length from `DTSTART`/`DTEND` (RFC 5545 §3.6.1).
    ///
    /// Both values must be the same kind: two dates (an all-day span, in whole
    /// **nominal days**), or two wall-clock times (nominal days plus the absolute
    /// remainder, so a span crossing a DST change is not a fixed second count —
    /// matching [`Duration`]'s day/second split). The wall-clock subtraction uses
    /// each value's local time, which is exact when both carry the same zone (the
    /// near-universal case for a single event). A date-vs-timed mismatch, or an
    /// `end` that precedes the start, is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`TimeError::OutOfRange`] if the two values are different kinds or
    /// `end` precedes the start.
    pub fn duration_until(&self, end: &Self) -> Result<Duration, TimeError> {
        match (self, end) {
            (Self::Date(start), Self::Date(end)) => {
                span_to_duration(end.as_date() - start.as_date())
            }
            (
                Self::Floating(start) | Self::Zoned { local: start, .. },
                Self::Floating(end) | Self::Zoned { local: end, .. },
            ) => span_to_duration(end.as_primitive() - start.as_primitive()),
            // DTSTART and DTEND must share a value type (RFC 5545 §3.8.2.2).
            _ => Err(TimeError::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> LocalDateTime {
        LocalDateTime::new(2021, 6, 1, 9, 30, 0).unwrap()
    }

    #[test]
    fn all_day_carries_no_zone() {
        let value = CalendarDateTime::Date(CalendarDate::new(2021, 6, 1).unwrap());
        assert!(value.is_all_day());
        assert!(value.zone().is_none());
        assert!(value.local().is_none());
    }

    #[test]
    fn floating_carries_no_zone_but_has_wall_clock() {
        let value = CalendarDateTime::Floating(local());
        assert!(value.is_floating());
        assert!(value.zone().is_none());
        assert_eq!(value.local(), Some(local()));
    }

    #[test]
    fn utc_is_zoned_etc_utc() {
        let value = CalendarDateTime::utc(local());
        assert!(!value.is_floating());
        assert!(!value.is_all_day());
        assert_eq!(value.zone(), Some(&TimeZoneId::utc()));
    }

    fn zoned(s: &str) -> CalendarDateTime {
        CalendarDateTime::Zoned {
            local: s.parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        }
    }

    fn dur(s: &str) -> Duration {
        s.parse().unwrap()
    }

    #[test]
    fn duration_between_zoned_times_is_the_wall_clock_span() {
        // The seed's one-off event: 10:00–11:00 → one hour.
        assert_eq!(
            zoned("2026-03-18T10:00:00")
                .duration_until(&zoned("2026-03-18T11:00:00"))
                .unwrap(),
            dur("PT1H")
        );
        // A moved standup instance: 14:00–14:30 → thirty minutes.
        assert_eq!(
            zoned("2026-01-26T14:00:00")
                .duration_until(&zoned("2026-01-26T14:30:00"))
                .unwrap(),
            dur("PT30M")
        );
    }

    #[test]
    fn duration_between_floating_times_spans_midnight_as_nominal_days() {
        let start = CalendarDateTime::Floating("2026-04-15T12:00:00".parse().unwrap());
        let end = CalendarDateTime::Floating("2026-04-17T13:30:00".parse().unwrap());
        // Two nominal days plus an hour and a half (P2DT1H30M), not PT49H30M.
        let span = start.duration_until(&end).unwrap();
        assert_eq!(span.days(), 2);
        assert_eq!(span.seconds(), 5_400);
    }

    #[test]
    fn duration_between_all_day_dates_is_whole_days() {
        // The seed's all-day event: DTSTART;VALUE=DATE:20260401 → DTEND:20260402.
        let start = CalendarDateTime::Date(CalendarDate::new(2026, 4, 1).unwrap());
        let end = CalendarDateTime::Date(CalendarDate::new(2026, 4, 2).unwrap());
        assert_eq!(start.duration_until(&end).unwrap(), dur("P1D"));
    }

    #[test]
    fn duration_rejects_mismatched_kinds_and_reversed_order() {
        let date = CalendarDateTime::Date(CalendarDate::new(2026, 4, 1).unwrap());
        let timed = zoned("2026-04-01T09:00:00");
        // A date cannot be subtracted from a timed value, or vice versa.
        assert_eq!(date.duration_until(&timed), Err(TimeError::OutOfRange));
        assert_eq!(timed.duration_until(&date), Err(TimeError::OutOfRange));
        // DTEND before DTSTART is rejected rather than silently zeroed.
        assert_eq!(
            zoned("2026-03-18T11:00:00").duration_until(&zoned("2026-03-18T10:00:00")),
            Err(TimeError::OutOfRange)
        );
    }

    #[test]
    fn zoned_keeps_its_zone() {
        let zone = TimeZoneId::iana("Europe/Amsterdam").unwrap();
        let value = CalendarDateTime::Zoned {
            local: local(),
            zone: zone.clone(),
        };
        assert_eq!(value.zone(), Some(&zone));
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(
            serde_json::from_str::<CalendarDateTime>(&json).unwrap(),
            value
        );
    }
}
