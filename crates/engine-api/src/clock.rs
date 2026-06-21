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
/// minutes) and to the strict `expiry > now` comparison the store makes. This is a
/// wall clock, so it is **not** monotonic — an NTP or manual adjustment can step it
/// backwards; lease safety across such a step rests on the TTL plus the `StaleLease`
/// re-claim in the sync loop (`store-and-sync.md`), not on any ordering guarantee
/// from this clock.
#[derive(Debug)]
pub(crate) struct SystemClock;

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
    fn now_is_a_plausible_recent_instant() {
        // A wall clock is not monotonic (NTP can step it), so we assert only that it
        // reads as a sane recent instant — not that successive reads increase.
        let now = SystemClock.now();
        assert!(now.year() >= 2024, "implausible clock: {now}");
        // The Debug form names the type (and exercises the derive).
        assert_eq!(format!("{SystemClock:?}"), "SystemClock");
    }
}
