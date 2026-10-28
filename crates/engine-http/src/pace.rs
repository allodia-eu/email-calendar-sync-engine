//! Holding an account to a rate its server bills in units of its own.

use std::time::Duration;

use tokio::time::Instant;

/// The share of a window's budget a rested account may spend at once: one tenth, so opening a
/// message on an idle account never waits, and the rest of the window paces what follows.
const BURST_SHARE: u32 = 10;

/// A budget of `units` per `per`, kept as a leaky bucket in virtual time.
///
/// Each call reserves its cost the moment it asks, so calls go out in the order they asked
/// whatever each costs, and a dear one is never starved by a stream of cheap ones. Any span of
/// length `per` then carries at most `units`: the burst plus what the sustained rate refills.
#[derive(Debug)]
pub(crate) struct Pace {
    units: u32,
    per: Duration,
    /// When everything reserved so far will have been paid for at the sustained rate.
    paid_off: Option<Instant>,
}

impl Pace {
    pub(crate) fn new(units: u32, per: Duration) -> Self {
        Self {
            units: units.max(1),
            per,
            paid_off: None,
        }
    }

    pub(crate) fn stated(&self) -> (u32, Duration) {
        (self.units, self.per)
    }

    /// Whether `units` per `per` is a slower rate than this one.
    pub(crate) fn is_slower(&self, units: u32, per: Duration) -> bool {
        u128::from(units.max(1)) * self.per.as_nanos() < u128::from(self.units) * per.as_nanos()
    }

    /// Adopts a stricter rate, keeping what is already reserved.
    pub(crate) fn restate(&mut self, units: u32, per: Duration) {
        self.units = units.max(1);
        self.per = per;
    }

    /// Reserves `cost` and returns how long the caller waits before sending.
    pub(crate) fn reserve(&mut self, cost: u32, now: Instant) -> Duration {
        let interval = self.interval();
        let start = self.paid_off.map_or(now, |paid_off| paid_off.max(now));
        let paid_off = start + interval * cost;
        self.paid_off = Some(paid_off);
        // A call may go once what is still owed, its own cost included, fits in the burst.
        paid_off
            .checked_sub(interval * self.burst())
            .map_or(Duration::ZERO, |ready| ready.saturating_duration_since(now))
    }

    fn burst(&self) -> u32 {
        (self.units / BURST_SHARE).max(1)
    }

    /// The time one unit takes to pay off at the sustained rate, rounded up so the rate never
    /// runs over what was stated.
    fn interval(&self) -> Duration {
        let sustained = u128::from(self.units - self.burst()).max(1);
        let nanos = self.per.as_nanos().div_ceil(sustained);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}
