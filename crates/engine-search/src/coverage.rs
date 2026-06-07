//! Assembling [`SearchCoverage`] from per-scope facts.
//!
//! A query spans one or more scopes (mailboxes, calendars, accounts). For each,
//! the executor knows three things (`search-coverage.md`): the local object/index
//! state, how far on-demand occurrence expansion reached, and whether a provider
//! search ran and called itself exhaustive. This module folds those per-scope
//! [`SearchCoverage`] values into the single coverage the answer carries.
//!
//! Assembly has two steps, both fixed by `search-coverage.md`:
//!
//! 1. **Remote compensation, per scope.** When a provider search for a scope
//!    reports an *exhaustive* result, that scope contributes no local or temporal
//!    gap — the provider searched its own full corpus (and a CalDAV time-range
//!    `REPORT` expands recurrence server-side). A non-exhaustive or absent remote
//!    search leaves the scope's gaps untouched. Either way the `remote` provenance
//!    is preserved, so compensation never hides that augmentation happened.
//! 2. **Conservative roll-up.** The compensated scopes combine via
//!    [`SearchCoverage::roll_up`]: gap flags OR together and bounded ranges
//!    intersect, so one incomplete scope makes the whole answer incomplete.
//!
//! Doing compensation here — not inside [`SearchCoverage::is_complete`] — keeps
//! `is_complete` a plain conjunction that composes correctly across scopes.

use engine_core::coverage::{LocalCoverage, RemoteCoverage, SearchCoverage, TemporalCoverage};

/// Compensates one scope's coverage for an exhaustive remote search.
fn compensate(scope: SearchCoverage) -> SearchCoverage {
    match scope.remote {
        RemoteCoverage::Augmented { exhaustive: true } => SearchCoverage {
            local: LocalCoverage::default(),
            temporal: TemporalCoverage::Full,
            remote: scope.remote,
        },
        _ => scope,
    }
}

/// Assembles per-scope coverage into one answer's [`SearchCoverage`].
///
/// Each scope is remote-compensated, then the scopes are conservatively rolled
/// up. An empty input rolls up to a complete, local-only answer (no scope reports
/// a gap).
#[must_use]
pub fn assemble(per_scope: impl IntoIterator<Item = SearchCoverage>) -> SearchCoverage {
    SearchCoverage::roll_up(per_scope.into_iter().map(compensate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::coverage::TimeRange;

    fn unsynced() -> SearchCoverage {
        SearchCoverage {
            local: LocalCoverage {
                unsynced_objects: true,
                unindexed_content: false,
            },
            ..SearchCoverage::complete()
        }
    }

    fn range(start: &str, end: &str) -> TimeRange {
        TimeRange::new(start.parse().unwrap(), end.parse().unwrap())
    }

    #[test]
    fn no_scopes_is_vacuously_complete() {
        let coverage = assemble(std::iter::empty());
        assert!(coverage.is_complete());
        assert_eq!(coverage.remote, RemoteCoverage::LocalOnly);
    }

    #[test]
    fn a_fully_local_scope_stays_complete() {
        let coverage = assemble([SearchCoverage::complete()]);
        assert!(coverage.is_complete());
        assert_eq!(coverage.remote, RemoteCoverage::LocalOnly);
    }

    #[test]
    fn exhaustive_remote_clears_the_scope_gap() {
        // A scope with un-synced objects, augmented by an exhaustive provider
        // search, contributes no residual gap; augmentation is still recorded.
        let scope = SearchCoverage {
            remote: RemoteCoverage::Augmented { exhaustive: true },
            ..unsynced()
        };
        let coverage = assemble([scope]);
        assert!(coverage.is_complete());
        assert!(!coverage.local.unsynced_objects);
        assert_eq!(
            coverage.remote,
            RemoteCoverage::Augmented { exhaustive: true }
        );
    }

    #[test]
    fn non_exhaustive_remote_leaves_the_residual_gap() {
        let scope = SearchCoverage {
            remote: RemoteCoverage::Augmented { exhaustive: false },
            ..unsynced()
        };
        let coverage = assemble([scope]);
        assert!(!coverage.is_complete());
        assert!(coverage.local.unsynced_objects);
        assert_eq!(
            coverage.remote,
            RemoteCoverage::Augmented { exhaustive: false }
        );
    }

    #[test]
    fn one_windowed_scope_makes_the_whole_answer_incomplete() {
        let windowed = SearchCoverage {
            temporal: TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            },
            ..SearchCoverage::complete()
        };
        let coverage = assemble([SearchCoverage::complete(), windowed]);
        assert!(!coverage.is_complete());
        assert_eq!(
            coverage.temporal,
            TemporalCoverage::Bounded {
                covered: range("2025-01-01T00:00:00Z", "2027-01-01T00:00:00Z"),
            }
        );
    }

    #[test]
    fn exhaustive_remote_does_not_rescue_a_different_incomplete_scope() {
        // One scope is exhaustively augmented (clears its own gap); another scope
        // is locally windowed and untouched. The roll-up stays incomplete.
        let augmented = SearchCoverage {
            remote: RemoteCoverage::Augmented { exhaustive: true },
            ..unsynced()
        };
        let windowed = SearchCoverage {
            local: LocalCoverage {
                unsynced_objects: false,
                unindexed_content: true,
            },
            ..SearchCoverage::complete()
        };
        let coverage = assemble([augmented, windowed]);
        assert!(!coverage.is_complete());
        assert!(coverage.local.unindexed_content);
        // Augmentation from the first scope is still recorded.
        assert_eq!(
            coverage.remote,
            RemoteCoverage::Augmented { exhaustive: true }
        );
    }
}
