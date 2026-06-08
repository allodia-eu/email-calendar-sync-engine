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

use engine_core::time::{TimeError, UtcDateTime};

pub use engine_store::{OccurrenceRow, TzdataVersion};
pub use expand::expand;

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
