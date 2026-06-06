//! Search coverage.
//!
//! A search answer must tell the caller how complete it is and why it might be
//! missing matches. Completeness is several independent axes, any subset of
//! which can apply to one query (`search-coverage.md`, authoritative):
//!
//! - **local** — object and content availability (partial sync);
//! - **temporal** — recurrence-horizon coverage for time-range queries;
//! - **remote** — whether a provider search contributed.
//!
//! Remote augmentation is compensated into the `local`/`temporal` axes at
//! assembly time, so [`SearchCoverage::is_complete`] is a plain conjunction over
//! those two; `remote` is provenance only. A multi-scope answer is the
//! conservative roll-up: gap flags are OR-ed and bounded ranges intersected.

use serde::{Deserialize, Serialize};

use crate::time::UtcDateTime;

/// A closed interval of instants, possibly empty.
///
/// An empty range (`start > end`, see [`TimeRange::is_empty`]) arises when
/// intersecting disjoint ranges and denotes "no instant is covered".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeRange {
    start: UtcDateTime,
    end: UtcDateTime,
}

impl TimeRange {
    /// Creates a range `[start, end]`. If `start > end` the range is empty.
    #[must_use]
    pub fn new(start: UtcDateTime, end: UtcDateTime) -> Self {
        Self { start, end }
    }

    /// The inclusive start.
    #[must_use]
    pub fn start(&self) -> UtcDateTime {
        self.start
    }

    /// The inclusive end.
    #[must_use]
    pub fn end(&self) -> UtcDateTime {
        self.end
    }

    /// Returns `true` if the range covers no instant.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start > self.end
    }

    /// Returns the overlap of two ranges, which may be empty.
    #[must_use]
    pub fn intersection(&self, other: &TimeRange) -> TimeRange {
        TimeRange::new(self.start.max(other.start), self.end.min(other.end))
    }
}

/// Local object and content availability across the query's scopes. Both flags
/// `false` means the local corpus was fully searchable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalCoverage {
    /// Some in-scope objects are not local yet (backfill incomplete, or a
    /// retention window excludes them).
    pub unsynced_objects: bool,
    /// Some in-scope objects are present as metadata only; their bodies or
    /// attachments were not indexed, so text matches on them are missed.
    pub unindexed_content: bool,
}

/// Time-range coverage for results that depend on recurrence expansion.
/// Always [`TemporalCoverage::Full`] for queries that do not expand occurrences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemporalCoverage {
    /// The requested range is covered (within the horizon, or expanded on demand
    /// up to the host's cap).
    Full,
    /// The requested range exceeds the expansion cap; recurring instances outside
    /// `covered` are missing.
    Bounded {
        /// The sub-range that is trustworthy.
        covered: TimeRange,
    },
}

/// Whether a provider-side search contributed to the answer. Informational: any
/// residual gap is already reflected in the local/temporal axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteCoverage {
    /// Local data only.
    LocalOnly,
    /// A provider search augmented the answer.
    Augmented {
        /// What the provider reported about its own result's exhaustiveness.
        exhaustive: bool,
    },
}

/// How complete a search answer is, and why it might be missing matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchCoverage {
    /// Local availability axis.
    pub local: LocalCoverage,
    /// Temporal (recurrence-horizon) axis.
    pub temporal: TemporalCoverage,
    /// Remote-augmentation provenance.
    pub remote: RemoteCoverage,
}

impl SearchCoverage {
    /// A fully complete, fully local answer.
    #[must_use]
    pub fn complete() -> Self {
        Self {
            local: LocalCoverage::default(),
            temporal: TemporalCoverage::Full,
            remote: RemoteCoverage::LocalOnly,
        }
    }

    /// True when no axis reports a known gap. Remote augmentation is already
    /// compensated into `local`/`temporal`, so it does not appear here.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.local.unsynced_objects
            && !self.local.unindexed_content
            && matches!(self.temporal, TemporalCoverage::Full)
    }

    /// Conservatively rolls up per-scope coverage: gap flags are OR-ed, bounded
    /// `covered` ranges are intersected, and remote augmentation is recorded if
    /// any scope used it (exhaustive only if every augmented scope was).
    #[must_use]
    pub fn roll_up(items: impl IntoIterator<Item = SearchCoverage>) -> SearchCoverage {
        let mut local = LocalCoverage::default();
        let mut covered: Option<TimeRange> = None;
        let mut augmented = false;
        let mut remote_exhaustive = true;
        for item in items {
            local.unsynced_objects |= item.local.unsynced_objects;
            local.unindexed_content |= item.local.unindexed_content;
            if let TemporalCoverage::Bounded { covered: range } = item.temporal {
                covered = Some(match covered {
                    None => range,
                    Some(acc) => acc.intersection(&range),
                });
            }
            if let RemoteCoverage::Augmented { exhaustive } = item.remote {
                augmented = true;
                remote_exhaustive &= exhaustive;
            }
        }
        SearchCoverage {
            local,
            temporal: match covered {
                Some(covered) => TemporalCoverage::Bounded { covered },
                None => TemporalCoverage::Full,
            },
            remote: if augmented {
                RemoteCoverage::Augmented {
                    exhaustive: remote_exhaustive,
                }
            } else {
                RemoteCoverage::LocalOnly
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant(s: &str) -> UtcDateTime {
        s.parse().unwrap()
    }

    fn range(start: &str, end: &str) -> TimeRange {
        TimeRange::new(instant(start), instant(end))
    }

    #[test]
    fn time_range_accessors() {
        let r = range("2025-01-01T00:00:00Z", "2026-01-01T00:00:00Z");
        assert_eq!(r.start(), instant("2025-01-01T00:00:00Z"));
        assert_eq!(r.end(), instant("2026-01-01T00:00:00Z"));
        assert!(!r.is_empty());
    }

    #[test]
    fn fully_local_answer_is_complete() {
        let coverage = SearchCoverage::complete();
        assert!(coverage.is_complete());
        assert_eq!(coverage.remote, RemoteCoverage::LocalOnly);
    }

    #[test]
    fn unindexed_content_makes_answer_incomplete() {
        let mut coverage = SearchCoverage::complete();
        coverage.local.unindexed_content = true;
        assert!(!coverage.is_complete());
    }

    #[test]
    fn temporal_full_versus_bounded() {
        let within = SearchCoverage::complete();
        assert!(matches!(within.temporal, TemporalCoverage::Full));
        let beyond = SearchCoverage {
            temporal: TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            },
            ..SearchCoverage::complete()
        };
        assert!(!beyond.is_complete());
    }

    #[test]
    fn exhaustive_remote_clears_the_gap_it_covered() {
        // The executor compensated the gap into local/temporal; remote records
        // that augmentation happened.
        let coverage = SearchCoverage {
            local: LocalCoverage::default(),
            temporal: TemporalCoverage::Full,
            remote: RemoteCoverage::Augmented { exhaustive: true },
        };
        assert!(coverage.is_complete());
    }

    #[test]
    fn non_exhaustive_remote_leaves_residual_gap() {
        let coverage = SearchCoverage {
            local: LocalCoverage {
                unsynced_objects: true,
                unindexed_content: false,
            },
            temporal: TemporalCoverage::Full,
            remote: RemoteCoverage::Augmented { exhaustive: false },
        };
        assert!(!coverage.is_complete());
    }

    #[test]
    fn multi_scope_rolls_up_conservatively() {
        let complete = SearchCoverage::complete();
        let windowed = SearchCoverage {
            temporal: TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            },
            ..SearchCoverage::complete()
        };
        let rolled = SearchCoverage::roll_up([complete, windowed]);
        assert!(!rolled.is_complete());
        assert_eq!(
            rolled.temporal,
            TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            },
        );
    }

    #[test]
    fn bounded_ranges_intersect_on_roll_up() {
        let a = SearchCoverage {
            temporal: TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            },
            ..SearchCoverage::complete()
        };
        let b = SearchCoverage {
            temporal: TemporalCoverage::Bounded {
                covered: range("2026-01-01T00:00:00Z", "2028-01-01T00:00:00Z"),
            },
            ..SearchCoverage::complete()
        };
        let rolled = SearchCoverage::roll_up([a, b]);
        let expected = range("2026-01-01T00:00:00Z", "2027-01-01T00:00:00Z");
        assert!(!expected.is_empty());
        assert_eq!(
            rolled.temporal,
            TemporalCoverage::Bounded { covered: expected }
        );
    }

    #[test]
    fn disjoint_bounded_ranges_intersect_to_empty() {
        let a = TemporalCoverage::Bounded {
            covered: range("2025-01-01T00:00:00Z", "2025-06-01T00:00:00Z"),
        };
        let b = TemporalCoverage::Bounded {
            covered: range("2026-01-01T00:00:00Z", "2026-06-01T00:00:00Z"),
        };
        let rolled = SearchCoverage::roll_up([
            SearchCoverage {
                temporal: a,
                ..SearchCoverage::complete()
            },
            SearchCoverage {
                temporal: b,
                ..SearchCoverage::complete()
            },
        ]);
        let empty = range("2026-01-01T00:00:00Z", "2025-06-01T00:00:00Z");
        assert!(empty.is_empty());
        assert_eq!(
            rolled.temporal,
            TemporalCoverage::Bounded { covered: empty }
        );
    }

    #[test]
    fn roll_up_records_remote_augmentation() {
        let exhaustive = SearchCoverage {
            remote: RemoteCoverage::Augmented { exhaustive: true },
            ..SearchCoverage::complete()
        };
        let partial = SearchCoverage {
            remote: RemoteCoverage::Augmented { exhaustive: false },
            ..SearchCoverage::complete()
        };
        let rolled = SearchCoverage::roll_up([exhaustive, partial]);
        // Augmentation is recorded; exhaustive only if every augmented scope was.
        assert_eq!(
            rolled.remote,
            RemoteCoverage::Augmented { exhaustive: false }
        );
    }
}
