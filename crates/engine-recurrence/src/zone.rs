//! The bundled-tzdb bridge: resolve wall-clock times to UTC instants.
//!
//! All of `jiff` is confined to this module. Zones come **only** from the bundled,
//! version-pinned tzdb (the workspace pins `jiff` with `default-features = false` +
//! `tzdb-bundle-always`, so no system path is read). `to_zoned` uses jiff's
//! Compatible disambiguation, which matches RFC 5545's gap/fold handling.

use engine_core::time::{CalendarDate, Duration, LocalDateTime, TimeZoneId, UtcDateTime};
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Span, Timestamp, Zoned};

use crate::ExpandError;

/// A generous upper bound on a duration's nominal-day component, well under jiff's
/// `Span` day limit, so building the span cannot panic.
const MAX_DURATION_DAYS: i64 = 3_660_000;

/// Resolves an IANA zone name from the bundled tzdb.
///
/// # Errors
///
/// Returns [`ExpandError::UnsupportedZone`] if the name is absent from the bundle.
pub(crate) fn iana(name: &str) -> Result<TimeZone, ExpandError> {
    TimeZone::get(name).map_err(|_| ExpandError::UnsupportedZone(name.to_owned()))
}

/// The UTC zone (used for all-day values, which are zoneless).
pub(crate) fn utc() -> TimeZone {
    TimeZone::UTC
}

/// Every IANA zone name the bundled tzdb can resolve, sorted.
///
/// Listed straight from the bundled database (and confirmed resolvable through the same
/// [`iana`] path), so it is exactly the set the engine localizes and migrates against —
/// the enumeration counterpart of [`resolve_zone_id`].
pub(crate) fn available() -> Vec<String> {
    let mut names: Vec<String> = jiff::tz::db()
        .available()
        .map(|name| name.as_str().to_owned())
        .filter(|name| iana(name).is_ok())
        .collect();
    names.sort();
    names
}

/// Resolves a [`TimeZoneId`] to a bundled jiff zone, rejecting custom/embedded
/// zones (their expansion needs the iCalendar `VTIMEZONE` parser, a later step).
pub(crate) fn resolve_zone_id(id: &TimeZoneId) -> Result<TimeZone, ExpandError> {
    if id.is_iana() {
        iana(id.as_str())
    } else {
        Err(ExpandError::UnsupportedZone(id.as_str().to_owned()))
    }
}

/// Builds a civil date from engine components.
pub(crate) fn date(year: i32, month: u8, day: u8) -> Result<Date, ExpandError> {
    let year = i16::try_from(year).map_err(|_| ExpandError::OutOfRange)?;
    let month = i8::try_from(month).map_err(|_| ExpandError::OutOfRange)?;
    let day = i8::try_from(day).map_err(|_| ExpandError::OutOfRange)?;
    Date::new(year, month, day).map_err(|_| ExpandError::OutOfRange)
}

/// The civil date of a [`LocalDateTime`].
pub(crate) fn local_date(local: LocalDateTime) -> Result<Date, ExpandError> {
    date(local.year(), local.month(), local.day())
}

/// The civil date of an all-day [`CalendarDate`].
pub(crate) fn calendar_date(value: CalendarDate) -> Result<Date, ExpandError> {
    date(value.year(), value.month(), value.day())
}

/// Midnight (00:00:00) — the start-of-day time used to resolve an all-day
/// [`CalendarDate`] to an instant in a display zone.
pub(crate) fn midnight() -> Time {
    Time::midnight()
}

/// The civil time-of-day of a [`LocalDateTime`].
pub(crate) fn local_time(local: LocalDateTime) -> Result<Time, ExpandError> {
    let hour = i8::try_from(local.hour()).map_err(|_| ExpandError::OutOfRange)?;
    let minute = i8::try_from(local.minute()).map_err(|_| ExpandError::OutOfRange)?;
    let second = i8::try_from(local.second()).map_err(|_| ExpandError::OutOfRange)?;
    let nanos = i32::try_from(local.nanosecond()).map_err(|_| ExpandError::OutOfRange)?;
    Time::new(hour, minute, second, nanos).map_err(|_| ExpandError::OutOfRange)
}

/// Resolves a wall-clock instant in `zone` to a UTC instant.
///
/// # Errors
///
/// Returns [`ExpandError::OutOfRange`] if the value falls outside representable
/// time.
pub(crate) fn resolve(zone: &TimeZone, dt: DateTime) -> Result<UtcDateTime, ExpandError> {
    let zoned = zone.to_zoned(dt).map_err(|_| ExpandError::OutOfRange)?;
    timestamp_to_utc(zoned.timestamp())
}

/// Resolves a wall-clock start in `zone` plus a duration into `(start, end)` UTC
/// instants. The duration's nominal days are added in calendar time (DST-aware),
/// its time component in absolute time (`Duration`).
///
/// # Errors
///
/// Returns [`ExpandError::OutOfRange`] if a value falls outside representable time.
pub(crate) fn resolve_range(
    zone: &TimeZone,
    dt: DateTime,
    duration: Duration,
) -> Result<(UtcDateTime, UtcDateTime), ExpandError> {
    let start = zone.to_zoned(dt).map_err(|_| ExpandError::OutOfRange)?;
    let end = add_duration(&start, duration)?;
    Ok((
        timestamp_to_utc(start.timestamp())?,
        timestamp_to_utc(end.timestamp())?,
    ))
}

/// Adds a [`Duration`] to a zoned start: nominal days in calendar time, the time
/// component in absolute time.
fn add_duration(start: &Zoned, duration: Duration) -> Result<Zoned, ExpandError> {
    if duration.is_zero() {
        return Ok(start.clone());
    }
    let days = i64::try_from(duration.days()).map_err(|_| ExpandError::OutOfRange)?;
    if days > MAX_DURATION_DAYS {
        return Err(ExpandError::OutOfRange);
    }
    let seconds = i64::try_from(duration.seconds()).map_err(|_| ExpandError::OutOfRange)?;
    let nanos = i32::try_from(duration.nanoseconds()).map_err(|_| ExpandError::OutOfRange)?;
    let mut zoned = start.clone();
    if days != 0 {
        zoned = zoned
            .checked_add(Span::new().days(days))
            .map_err(|_| ExpandError::OutOfRange)?;
    }
    if seconds != 0 || nanos != 0 {
        zoned = zoned
            .checked_add(SignedDuration::new(seconds, nanos))
            .map_err(|_| ExpandError::OutOfRange)?;
    }
    Ok(zoned)
}

/// Combines a civil date and time-of-day into a civil date-time.
pub(crate) fn at(date: Date, time: Time) -> DateTime {
    DateTime::from_parts(date, time)
}

/// Renders a jiff [`Timestamp`] as an engine [`UtcDateTime`], preserving
/// sub-second precision via the canonical `…Z` text form.
fn timestamp_to_utc(ts: Timestamp) -> Result<UtcDateTime, ExpandError> {
    let z = ts.to_zoned(TimeZone::UTC);
    let text = if z.subsec_nanosecond() == 0 {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            z.year(),
            z.month(),
            z.day(),
            z.hour(),
            z.minute(),
            z.second()
        )
    } else {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
            z.year(),
            z.month(),
            z.day(),
            z.hour(),
            z.minute(),
            z.second(),
            z.subsec_nanosecond()
        )
    };
    text.parse().map_err(ExpandError::from)
}
