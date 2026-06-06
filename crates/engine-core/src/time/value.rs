//! The four-case scheduled-time value.

use serde::{Deserialize, Serialize};

use super::{CalendarDate, LocalDateTime, TimeZoneId};

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
