//! The host wall-clock [`Clock`] for production engines.

use engine_core::time::UtcDateTime;
use engine_store::Clock;
use time::OffsetDateTime;

/// A [`Clock`] backed by the host's real-time clock.
///
/// `engine-store` ships only
/// [`ManualClock`](engine_store::ManualClock) for deterministic tests; a real host
/// needs wall-clock time to expire leases. This is that source, kept in
/// `engine-api` so `engine-store` never reads the system clock itself — the
/// engine's time source stays one injected seam (`north-star.md`).
///
/// Resolution is whole seconds: [`UtcDateTime`] is rebuilt from civil components,
/// and sub-second precision is irrelevant to lease liveness (TTLs run seconds to
/// minutes) and to the strict `expiry > now` comparison the store makes. Truncation
/// never moves the clock backwards across a second boundary, so lease ordering
/// holds.
#[derive(Debug)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UtcDateTime {
        let now = OffsetDateTime::now_utc();
        UtcDateTime::new(
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        )
        .expect("a civil UTC time from the system clock is always representable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_recent_and_does_not_run_backwards() {
        let clock = SystemClock;
        let first = clock.now();
        // The engine was written well after 2024; any sane wall clock is past it.
        assert!(first.year() >= 2024, "implausible clock: {first}");
        // Whole-second resolution: successive reads never decrease.
        assert!(clock.now() >= first);
        // The Debug form names the type (and exercises the derive).
        assert_eq!(format!("{SystemClock:?}"), "SystemClock");
    }
}
