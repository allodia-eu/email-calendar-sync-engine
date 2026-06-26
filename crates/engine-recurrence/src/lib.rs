//! `engine-recurrence` — deterministic recurrence/occurrence expansion.
//!
//! This crate turns a normalized [`Event`](engine_core::calendar::Event) into the materialized
//! [`OccurrenceRow`]s that back calendar time-range search (`store-and-sync.md`,
//! `search.md`). It is the "recurrence/index layer" the store contract refers to:
//! the store is mechanical and never expands recurrence, so [`expand`] runs in
//! pure engine code *before* the store call and its output is carried in
//! `DerivedWrite::occurrences`.
//!
//! # Determinism
//!
//! Expansion resolves wall-clock times to UTC instants through IANA zones, and a
//! user's devices must expand **identically** (`calendar-semantics.md`). So this
//! crate uses a **bundled, version-pinned** copy of the IANA tz database via
//! `jiff` + `jiff-tzdb`, configured (in the workspace `Cargo.toml`) with
//! `default-features = false` + `tzdb-bundle-always` so `jiff` never consults the
//! host's `/usr/share/zoneinfo`, `TZDIR`, or system zone. The bundled release is
//! [`tzdata_version`]; every [`OccurrenceRow`] records the version it was expanded
//! under, so a tzdata bump can find and re-expand exactly the affected rows.
//!
//! # Supported recurrence subset
//!
//! Recurrence rules are stored structurally ([`engine_core::calendar::RecurrenceRule`]),
//! covering all of RFC 5545's `RRULE`. The first-pass expander implements the
//! common subset and rejects the rest with [`ExpandError::UnsupportedRule`] /
//! [`ExpandError::UnsupportedZone`] so callers can preserve the master event
//! without silently dropping instances. Supported:
//!
//! - **Frequencies:** `DAILY`, `WEEKLY`, `MONTHLY`, `YEARLY`.
//! - **`INTERVAL`** (every *n* periods).
//! - **Termination:** `COUNT`, `UNTIL`, and unbounded (capped by the horizon).
//! - **`BYDAY`** including an nth-of-period (e.g. last Friday) for `MONTHLY`, and
//!   for `YEARLY` when scoped by `BYMONTH`.
//! - **`BYMONTHDAY`** including negatives (e.g. `-1` = last day of month).
//! - **`BYMONTH`**.
//! - **`WKST`** (week start), affecting `WEEKLY` + `INTERVAL` + `BYDAY`.
//! - **Per-instance overrides** ([`engine_core::calendar::RecurrenceOverride`]):
//!   exclusion (EXDATE), cancellation (`status: cancelled`), and a moved instance
//!   (a patched `start`/`duration`); an override on a non-rule instant adds it
//!   (RDATE-like).
//! - **Floating** times (resolved through the caller's `host_zone`) and **all-day**
//!   (zoneless UTC-midnight, zone-invariant).
//!
//! Staged (return an error, not expanded this pass):
//!
//! - `BYYEARDAY`, `BYWEEKNO`, `BYSETPOS`, and year-relative nth `BYDAY`.
//! - Sub-daily frequencies (`HOURLY`/`MINUTELY`/`SECONDLY`).
//! - `RSCALE` / non-Gregorian recurrence (preserved raw, never expanded —
//!   `calendar-semantics.md`).
//! - Custom / embedded-`VTIMEZONE` zones ([`engine_core::time::TimeZoneId::Custom`]);
//!   these need the iCalendar parser (a later provider step).
//! - Cross-object master/override-instance reconciliation: [`expand`] is a pure
//!   single-`Event` function. A recurring master expands its inline overrides; a
//!   standalone override-instance `Event` (its `recurrence_id` set) expands to its
//!   own single occurrence. Deduplicating a master against sibling override
//!   objects is the sync layer's job.

mod expand;
mod rule;
mod zone;

use engine_core::time::{CalendarDateTime, LocalDateTime, TimeError, TimeZoneId, UtcDateTime};

pub use engine_store::{OccurrenceRow, TzdataVersion};
pub use expand::expand;

/// Resolves a wall-clock value through `zone_id` to its absolute UTC instant.
fn resolve_local(local: &LocalDateTime, zone_id: &TimeZoneId) -> Result<UtcDateTime, ExpandError> {
    let tz = zone::resolve_zone_id(zone_id)?;
    zone::resolve(
        &tz,
        zone::at(zone::local_date(*local)?, zone::local_time(*local)?),
    )
}

/// Resolves a scheduled [`CalendarDateTime`] to its absolute UTC instant, when it
/// has one.
///
/// A [`Zoned`](CalendarDateTime::Zoned) value — including UTC, which the engine
/// stores as `Etc/UTC` — resolves through its IANA zone in the bundled,
/// version-pinned tzdb (the same path [`expand`] uses, so a host that localizes a
/// single event agrees with the materialized occurrence rows). A
/// [`Floating`](CalendarDateTime::Floating) or all-day
/// [`Date`](CalendarDateTime::Date) value has no fixed instant without an external
/// zone, so this returns `Ok(None)` — the host renders those as wall-clock or date
/// text. This is the read-side counterpart hosts use to display a stored event's
/// start in the device's local zone, regardless of the zone the event was authored
/// in (`calendar-semantics.md`). When a single ordering key is needed for *every*
/// value (e.g. sorting an agenda that mixes floating/all-day with zoned), use
/// [`resolve_instant_in`] with a display zone instead.
///
/// # Errors
///
/// Returns [`ExpandError::UnsupportedZone`] if the value carries a custom or
/// embedded `VTIMEZONE` zone that the bundled tzdb cannot resolve, or
/// [`ExpandError::OutOfRange`] if the instant falls outside representable time.
pub fn resolve_instant(value: &CalendarDateTime) -> Result<Option<UtcDateTime>, ExpandError> {
    match value {
        CalendarDateTime::Date(_) | CalendarDateTime::Floating(_) => Ok(None),
        CalendarDateTime::Zoned { local, zone } => resolve_local(local, zone).map(Some),
    }
}

/// Resolves a scheduled [`CalendarDateTime`] to an absolute UTC instant in the
/// context of a `host_zone`, so *every* value has one — the total-order key a host
/// uses to sort an agenda that mixes kinds.
///
/// Unlike [`resolve_instant`], a [`Floating`](CalendarDateTime::Floating) value
/// resolves through `host_zone` (it has no zone of its own) and an all-day
/// [`Date`](CalendarDateTime::Date) resolves to that zone's local midnight, so an
/// all-day event sorts at the start of its day alongside that day's timed events. A
/// [`Zoned`](CalendarDateTime::Zoned) value still resolves through its *own* zone —
/// `host_zone` is only the fallback for zoneless values.
///
/// # Errors
///
/// Returns [`ExpandError::UnsupportedZone`] if the applicable zone is a custom or
/// embedded `VTIMEZONE` the bundled tzdb cannot resolve, or
/// [`ExpandError::OutOfRange`] if the instant falls outside representable time.
pub fn resolve_instant_in(
    value: &CalendarDateTime,
    host_zone: &TimeZoneId,
) -> Result<UtcDateTime, ExpandError> {
    match value {
        CalendarDateTime::Date(date) => {
            let tz = zone::resolve_zone_id(host_zone)?;
            zone::resolve(&tz, zone::at(zone::calendar_date(*date)?, zone::midnight()))
        }
        CalendarDateTime::Floating(local) => resolve_local(local, host_zone),
        CalendarDateTime::Zoned { local, zone } => resolve_local(local, zone),
    }
}

/// Returns `true` if `zone` is a named IANA zone the bundled tzdb can resolve.
///
/// A host validates a user-picked or device-reported zone through this before
/// adopting it, so it never stores a display zone the engine cannot localize
/// against. A custom/embedded `VTIMEZONE` ([`TimeZoneId::Custom`]) is never
/// resolvable here and returns `false`.
#[must_use]
pub fn is_supported_zone(zone: &TimeZoneId) -> bool {
    zone::resolve_zone_id(zone).is_ok()
}

/// The bundled IANA tzdata release this build expands under.
///
/// Recorded on every [`OccurrenceRow`] so a tzdata-version bump can find and
/// re-expand exactly the occurrences produced under an older release. The bundle
/// always carries a version (we pin `tzdb-bundle-always`); `"unknown"` is a
/// defensive fallback that never occurs in a normal build.
#[must_use]
pub fn tzdata_version() -> TzdataVersion {
    TzdataVersion::new(jiff_tzdb::VERSION.unwrap_or("unknown"))
}

/// The half-open UTC window `[start, end)` within which occurrences are
/// materialized.
///
/// Occurrences are emitted only when their start instant falls in this window.
/// The host configures the rolling horizon; advancing it materializes further out
/// through the maintenance path (`store-and-sync.md`). A recurrence that would
/// continue past `end` is simply not materialized past it (no silent infinite
/// expansion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Horizon {
    start: UtcDateTime,
    end: UtcDateTime,
}

impl Horizon {
    /// Creates a horizon spanning `[start, end)`.
    ///
    /// # Errors
    ///
    /// Returns [`ExpandError::EmptyHorizon`] if `start` is not strictly before
    /// `end`.
    pub fn new(start: UtcDateTime, end: UtcDateTime) -> Result<Self, ExpandError> {
        if start >= end {
            return Err(ExpandError::EmptyHorizon);
        }
        Ok(Self { start, end })
    }

    /// The inclusive lower bound.
    #[must_use]
    pub fn start(self) -> UtcDateTime {
        self.start
    }

    /// The exclusive upper bound.
    #[must_use]
    pub fn end(self) -> UtcDateTime {
        self.end
    }
}

/// Why [`expand`] could not materialize an event's occurrences.
///
/// Unsupported rules and zones are surfaced (not silently skipped) so the caller
/// can preserve the master event without dropping instances on the floor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExpandError {
    /// The horizon's `start` was not strictly before its `end`.
    #[error("horizon start must be strictly before end")]
    EmptyHorizon,
    /// A recurrence-rule part outside the supported subset (see the crate docs).
    #[error("unsupported recurrence rule: {0}")]
    UnsupportedRule(&'static str),
    /// A zone that cannot be resolved deterministically: a custom/embedded
    /// `VTIMEZONE`, or an IANA name absent from the bundled tzdb.
    #[error("unsupported or unknown time zone: {0}")]
    UnsupportedZone(String),
    /// An override patch carried a malformed occurrence-relevant value
    /// (`start`/`duration`/`status`).
    #[error("invalid recurrence override for {recurrence_id}: {reason}")]
    InvalidOverride {
        /// The recurrence id (original instant) whose override was malformed.
        recurrence_id: String,
        /// What was wrong.
        reason: &'static str,
    },
    /// A date, time, or duration fell outside the representable range.
    #[error("date/time out of representable range")]
    OutOfRange,
    /// Generation exceeded the safety cap (a rule that matches implausibly often
    /// over the horizon).
    #[error("recurrence expansion exceeded the {0} instance safety cap")]
    TooManyInstances(usize),
}

impl From<TimeError> for ExpandError {
    fn from(_: TimeError) -> Self {
        Self::OutOfRange
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::time::{CalendarDate, LocalDateTime, TimeZoneId};

    fn local(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> LocalDateTime {
        LocalDateTime::new(year, month, day, hour, minute, 0).unwrap()
    }

    #[test]
    fn utc_resolves_to_the_same_instant() {
        // UTC is `Etc/UTC`; resolving it is the identity (wall clock == instant).
        let value = CalendarDateTime::utc(local(2026, 6, 27, 22, 0));
        assert_eq!(
            resolve_instant(&value).unwrap(),
            Some("2026-06-27T22:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn a_named_zone_resolves_through_tzdata() {
        // The user's real bug: an Amsterdam 22:00 event in summer (CEST, UTC+2)
        // is the instant 20:00Z — so a host localizes it correctly anywhere, not
        // just for a device that happens to share the authoring zone.
        let value = CalendarDateTime::Zoned {
            local: local(2026, 6, 27, 22, 0),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        assert_eq!(
            resolve_instant(&value).unwrap(),
            Some("2026-06-27T20:00:00Z".parse().unwrap())
        );
        // The same wall clock in winter (CET, UTC+1) is a different instant.
        let winter = CalendarDateTime::Zoned {
            local: local(2026, 1, 27, 22, 0),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        assert_eq!(
            resolve_instant(&winter).unwrap(),
            Some("2026-01-27T21:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn floating_and_all_day_have_no_instant() {
        // No zone → no fixed instant; the host renders these as wall-clock/date.
        let floating = CalendarDateTime::Floating(local(2026, 6, 27, 9, 0));
        assert_eq!(resolve_instant(&floating).unwrap(), None);
        let all_day = CalendarDateTime::Date(CalendarDate::new(2026, 6, 27).unwrap());
        assert_eq!(resolve_instant(&all_day).unwrap(), None);
    }

    #[test]
    fn a_custom_zone_is_unsupported() {
        // A custom/embedded VTIMEZONE cannot be resolved from the bundled tzdb.
        let value = CalendarDateTime::Zoned {
            local: local(2026, 6, 27, 22, 0),
            zone: TimeZoneId::custom("/example.com/Custom").unwrap(),
        };
        assert!(matches!(
            resolve_instant(&value),
            Err(ExpandError::UnsupportedZone(_))
        ));
    }

    #[test]
    fn resolve_instant_in_resolves_floating_through_the_host_zone() {
        // A floating wall-clock has no zone, so it resolves through the display zone:
        // 09:00 in Amsterdam (summer, UTC+2) is 07:00Z.
        let floating = CalendarDateTime::Floating(local(2026, 6, 27, 9, 0));
        let ams = TimeZoneId::iana("Europe/Amsterdam").unwrap();
        assert_eq!(
            resolve_instant_in(&floating, &ams).unwrap(),
            "2026-06-27T07:00:00Z".parse().unwrap()
        );
        // The same floating time resolves differently under a different host zone
        // (New York summer, UTC-4): 09:00 -> 13:00Z.
        let ny = TimeZoneId::iana("America/New_York").unwrap();
        assert_eq!(
            resolve_instant_in(&floating, &ny).unwrap(),
            "2026-06-27T13:00:00Z".parse().unwrap()
        );
    }

    #[test]
    fn resolve_instant_in_uses_an_all_day_value_at_host_zone_midnight() {
        // An all-day value sorts at the start of its day in the display zone:
        // 2026-06-27 midnight in Amsterdam (summer, UTC+2) is 2026-06-26T22:00Z.
        let all_day = CalendarDateTime::Date(CalendarDate::new(2026, 6, 27).unwrap());
        let ams = TimeZoneId::iana("Europe/Amsterdam").unwrap();
        assert_eq!(
            resolve_instant_in(&all_day, &ams).unwrap(),
            "2026-06-26T22:00:00Z".parse().unwrap()
        );
    }

    #[test]
    fn resolve_instant_in_keeps_a_zoned_value_in_its_own_zone() {
        // A zoned value ignores the host zone — its own zone wins. Amsterdam 22:00
        // is 20:00Z whether the host zone is New York or anything else.
        let zoned = CalendarDateTime::Zoned {
            local: local(2026, 6, 27, 22, 0),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        let ny = TimeZoneId::iana("America/New_York").unwrap();
        assert_eq!(
            resolve_instant_in(&zoned, &ny).unwrap(),
            "2026-06-27T20:00:00Z".parse().unwrap()
        );
    }

    #[test]
    fn supported_zone_accepts_iana_and_rejects_custom_or_unknown() {
        assert!(is_supported_zone(&TimeZoneId::utc()));
        assert!(is_supported_zone(
            &TimeZoneId::iana("Europe/Amsterdam").unwrap()
        ));
        // A syntactically-valid IANA name absent from the bundle is not supported.
        assert!(!is_supported_zone(
            &TimeZoneId::iana("Mars/Olympus").unwrap()
        ));
        // A custom/embedded VTIMEZONE is never resolvable here.
        assert!(!is_supported_zone(
            &TimeZoneId::custom("/example.com/Custom").unwrap()
        ));
    }
}
