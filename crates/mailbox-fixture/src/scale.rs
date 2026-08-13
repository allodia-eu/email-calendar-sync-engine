//! How large a mailbox a benchmark run builds.
//!
//! Three sizes, because they answer different questions. `10k` is the one a developer
//! runs while changing something — a build of seconds, enough to see a direction.
//! `100k` is the CI size: large enough that an O(mailbox) read is unmistakable, small
//! enough to fit a job. `400k` is the ~20 GB mailbox the design has to survive, and is
//! opt-in because building it is minutes, not seconds.

/// The environment variable that selects the fixture size.
pub const SCALE: &str = "ENGINE_BENCH_SCALE";

/// A benchmark mailbox size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// A short name for the size, used to label a table and a benchmark group.
    pub label: &'static str,
    /// How many messages the fixture holds.
    pub messages: usize,
}

/// The default when [`SCALE`] is unset: fast enough to run while iterating.
const SMALL: Scale = Scale {
    label: "10k messages",
    messages: 10_000,
};

/// Every size a run can select.
const SCALES: &[Scale] = &[
    SMALL,
    Scale {
        label: "100k messages",
        messages: 100_000,
    },
    Scale {
        label: "400k messages",
        messages: 400_000,
    },
];

impl Scale {
    /// The scale [`SCALE`] selects: `10k`, `100k`, `400k`, or a bare message count.
    /// Defaults to `10k` when the variable is unset or empty.
    ///
    /// # Panics
    ///
    /// Panics if the variable holds something that is neither a known label nor a
    /// positive integer — a silent fall back to the default would report a 10k number
    /// under a 400k heading, which is worse than not running at all.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(SCALE) {
            Ok(value) if !value.trim().is_empty() => Self::parse(value.trim()),
            _ => SMALL,
        }
    }

    /// Resolves one selector value.
    fn parse(value: &str) -> Self {
        if let Some(scale) = SCALES
            .iter()
            .find(|scale| scale.label.starts_with(value) && value.ends_with('k'))
        {
            return *scale;
        }
        let messages: usize = value.parse().unwrap_or_else(|_| {
            panic!("{SCALE}={value} is neither a known size (10k, 100k, 400k) nor a message count")
        });
        assert!(messages > 0, "{SCALE} must be a positive message count");
        Self {
            label: "custom size",
            messages,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SMALL, Scale};

    #[test]
    fn the_named_sizes_resolve() {
        assert_eq!(Scale::parse("10k"), SMALL);
        assert_eq!(Scale::parse("100k").messages, 100_000);
        assert_eq!(Scale::parse("400k").messages, 400_000);
    }

    #[test]
    fn a_bare_count_is_accepted_for_a_one_off_probe() {
        let scale = Scale::parse("2500");
        assert_eq!(scale.messages, 2_500);
        assert_eq!(scale.label, "custom size");
    }

    #[test]
    #[should_panic(expected = "neither a known size")]
    fn a_typo_fails_loudly_rather_than_silently_measuring_the_default() {
        // The failure this guards: `ENGINE_BENCH_SCALE=400K` quietly building 10k and
        // publishing the result under a 400k heading.
        let _ = Scale::parse("400K");
    }

    #[test]
    #[should_panic(expected = "positive message count")]
    fn zero_messages_is_rejected() {
        let _ = Scale::parse("0");
    }
}
