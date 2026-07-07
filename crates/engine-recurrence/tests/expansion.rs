//! Behavioral tests for recurrence/occurrence expansion.
//!
//! Locks the `calendar-semantics.md` "Required tests" reachable by the pure
//! expander (VTIMEZONE source, tzdata-version recording + determinism, floating vs
//! all-day) plus the supported RRULE subset and the override/exclusion/cancelled
//! rules.
//!
//! The cases live in the `expansion/` submodules declared below; this binary holds
//! the shared fixtures/helpers they reach via `super::`.

use core::num::NonZeroU32;

use engine_core::{
    calendar::{Event, Frequency, RecurrenceBound, RecurrenceRule},
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::{CalendarDateTime, LocalDateTime, TimeZoneId},
};
use engine_recurrence::{Horizon, OccurrenceRow, expand};

#[path = "expansion/determinism.rs"]
mod determinism;
#[path = "expansion/overrides_and_unsupported.rs"]
mod overrides_and_unsupported;
#[path = "expansion/rrule_subset.rs"]
mod rrule_subset;
#[path = "expansion/single_and_zone.rs"]
mod single_and_zone;

// --- helpers --------------------------------------------------------------

fn ldt(s: &str) -> LocalDateTime {
    s.parse().expect("valid local date-time")
}

fn instant(s: &str) -> engine_core::time::UtcDateTime {
    s.parse().expect("valid instant")
}

fn zoned(local: &str, zone: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: ldt(local),
        zone: TimeZoneId::iana(zone).expect("valid zone"),
    }
}

fn utc(local: &str) -> CalendarDateTime {
    zoned(local, "Etc/UTC")
}

/// The observer/device zone used for floating resolution (irrelevant to zoned and
/// all-day events).
fn host() -> TimeZoneId {
    TimeZoneId::utc()
}

fn event(start: CalendarDateTime) -> Event {
    Event::new(
        EventId::try_from("e1").expect("id"),
        Uid::new("u1").expect("uid"),
        Memberships::of_one(CalendarId::try_from("cal").expect("cal")),
        start,
    )
}

/// A wide horizon so COUNT/UNTIL/nth tests are never horizon-limited.
fn wide() -> Horizon {
    Horizon::new(
        instant("2000-01-01T00:00:00Z"),
        instant("2035-01-01T00:00:00Z"),
    )
    .expect("wide horizon")
}

fn rule(freq: Frequency) -> RecurrenceRule {
    RecurrenceRule::new(freq)
}

fn count(n: u32) -> RecurrenceBound {
    RecurrenceBound::Count(NonZeroU32::new(n).expect("non-zero"))
}

fn starts(occs: &[OccurrenceRow]) -> Vec<String> {
    occs.iter().map(|o| o.start.to_string()).collect()
}

fn expand_ok(ev: &Event, horizon: Horizon) -> Vec<OccurrenceRow> {
    expand(ev, &horizon, &host()).expect("expansion succeeds")
}
