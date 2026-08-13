//! The fixture's deterministic pseudo-random source.
//!
//! A benchmark fixture is only a yardstick if two runs of the same seed produce the
//! same mailbox byte for byte, on every platform. `rand`'s generators are not
//! guaranteed reproducible across releases, so the ~20 lines of SplitMix64 below are
//! the source instead: a fixed algorithm with no dependency, no global state, and no
//! platform-dependent word size.

/// A seeded SplitMix64 generator (Steele, Lea & Flood, 2014).
///
/// Chosen for being fully specified by two constants and one shift sequence: the
/// stream a seed produces is fixed forever, which is what a comparable baseline
/// needs. Not cryptographic, and never used for anything but fixture shape.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    /// Starts a stream at `seed`.
    #[must_use]
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 bits of the stream.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`, using the high bits (the well-mixed end) via a
    /// widening multiply, so the result is unbiased enough for fixture shape and
    /// needs no rejection loop.
    ///
    /// # Panics
    ///
    /// Panics if `bound` is zero.
    pub(crate) fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "bound must be positive");
        let scaled = u128::from(self.next_u64()) * bound as u128;
        usize::try_from(scaled >> 64).expect("a value below `bound` fits a usize")
    }

    /// Returns `true` with probability `percent`/100.
    pub(crate) fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// Picks one of `choices`.
    ///
    /// # Panics
    ///
    /// Panics if `choices` is empty.
    pub(crate) fn pick<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        &choices[self.below(choices.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        // The property the whole fixture rests on: a baseline captured today is
        // comparable to one captured after a refactor only if the mailbox is identical.
        let drawn: Vec<u64> = (0..8).map(|_| Rng::new(7).next_u64()).collect();
        assert!(drawn.iter().all(|value| *value == drawn[0]));

        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1_000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn distinct_seeds_diverge_immediately() {
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn below_stays_in_range_and_covers_it() {
        let mut rng = Rng::new(9);
        let mut seen = [false; 6];
        for _ in 0..500 {
            let value = rng.below(6);
            assert!(value < 6);
            seen[value] = true;
        }
        assert!(seen.iter().all(|hit| *hit), "every bucket is reachable");
        assert_eq!(rng.below(1), 0, "a bound of one is always zero");
    }

    #[test]
    fn chance_tracks_its_percentage() {
        let mut rng = Rng::new(3);
        let hits = (0..10_000).filter(|_| rng.chance(25)).count();
        assert!(
            (2_000..3_000).contains(&hits),
            "roughly a quarter, got {hits}"
        );
        assert!(!rng.chance(0), "zero percent never fires");
        assert!(rng.chance(100), "a hundred percent always fires");
    }

    #[test]
    fn pick_returns_a_member_of_the_slice() {
        let mut rng = Rng::new(11);
        let choices = ["a", "b", "c"];
        for _ in 0..100 {
            assert!(choices.contains(rng.pick(&choices)));
        }
    }
}
