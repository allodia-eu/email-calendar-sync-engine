//! The half-open UTC window `[start, end)` that bounds occurrence materialization
//! and the range reads over it, plus the window a store has actually materialized.

use super::{TimeError, TimeZoneId, UtcDateTime};

/// The half-open UTC window `[start, end)` within which occurrences are
/// materialized, and which a range read over them is bounded by.
///
/// Occurrences are emitted only when their start instant falls in this window.
/// The host configures the rolling horizon; advancing it materializes further out
/// through the maintenance path (`store-and-sync.md`). A recurrence that would
/// continue past `end` is simply not materialized past it (no silent infinite
/// expansion).
///
/// It lives here — in the tzdata-free value layer — rather than beside the
/// expander, because the store bounds its occurrence range reads by the same
/// window and cannot depend on `engine-recurrence` (which depends on it).
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
    /// Returns [`TimeError::EmptyRange`] if `start` is not strictly before `end`.
    pub fn new(start: UtcDateTime, end: UtcDateTime) -> Result<Self, TimeError> {
        if start >= end {
            return Err(TimeError::EmptyRange);
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

    /// Whether an occurrence spanning `[start, end)` overlaps this window.
    ///
    /// Half-open on both sides, so an occurrence that ends exactly when the window
    /// begins — or begins exactly when it ends — does **not** overlap. A
    /// zero-length occurrence (`start == end`) is treated as the point `start`, so
    /// it overlaps iff that point falls inside the window; the half-open rule alone
    /// would exclude one sitting exactly on `start()`, dropping a midnight
    /// zero-length event from the first day of every window that begins there.
    #[must_use]
    pub fn overlaps(self, start: UtcDateTime, end: UtcDateTime) -> bool {
        if start == end {
            return start >= self.start && start < self.end;
        }
        start < self.end && end > self.start
    }
}

/// The window a scope's occurrence rows have actually been **materialized over**, and the
/// zone they were resolved through.
///
/// The store owns this, because the occurrence rows are only meaningful *relative to it*:
/// an event has rows in `[start, end)` and provably none outside, and a floating event's
/// stored instant is only correct for the zone it was expanded under. Leaving it implicit
/// was a bug — a pass that re-derived one event over its *own* horizon deleted that event's
/// rows outside it and re-materialized only its own window, so a single changed event
/// silently lost every occurrence the host had already expanded (a moved event vanishing
/// from next month's grid while every unchanged event still rendered there).
///
/// So a sync or a post-write reconcile re-expands a changed event over the window the
/// **store** holds, never over a window the caller happened to pass. Only the explicit
/// maintenance path (`engine_sync::expand_calendar_horizon`) moves the window, which is
/// exactly the host call that re-expands *every* event to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionWindow {
    /// The horizon the rows span.
    pub horizon: Horizon,
    /// The zone a floating time was resolved through.
    pub zone: TimeZoneId,
}

impl ExpansionWindow {
    /// The window `horizon` resolved through `zone`.
    #[must_use]
    pub fn new(horizon: Horizon, zone: TimeZoneId) -> Self {
        Self { horizon, zone }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raw: &str) -> UtcDateTime {
        raw.parse().expect("valid instant")
    }

    fn window() -> Horizon {
        Horizon::new(at("2026-07-06T00:00:00Z"), at("2026-07-13T00:00:00Z")).expect("valid horizon")
    }

    #[test]
    fn rejects_an_empty_or_inverted_window() {
        let instant = at("2026-07-06T00:00:00Z");
        assert_eq!(Horizon::new(instant, instant), Err(TimeError::EmptyRange));
        assert_eq!(
            Horizon::new(at("2026-07-13T00:00:00Z"), instant),
            Err(TimeError::EmptyRange)
        );
    }

    #[test]
    fn overlap_is_half_open_at_both_ends() {
        let h = window();
        // Wholly inside.
        assert!(h.overlaps(at("2026-07-08T09:00:00Z"), at("2026-07-08T10:00:00Z")));
        // Straddling each edge, and spanning the whole window.
        assert!(h.overlaps(at("2026-07-05T23:00:00Z"), at("2026-07-06T01:00:00Z")));
        assert!(h.overlaps(at("2026-07-12T23:00:00Z"), at("2026-07-13T01:00:00Z")));
        assert!(h.overlaps(at("2026-06-01T00:00:00Z"), at("2026-08-01T00:00:00Z")));
        // Merely touching either edge does not overlap.
        assert!(!h.overlaps(at("2026-07-05T00:00:00Z"), at("2026-07-06T00:00:00Z")));
        assert!(!h.overlaps(at("2026-07-13T00:00:00Z"), at("2026-07-14T00:00:00Z")));
        // Wholly outside.
        assert!(!h.overlaps(at("2026-01-01T00:00:00Z"), at("2026-01-02T00:00:00Z")));
    }

    #[test]
    fn a_zero_length_occurrence_counts_as_its_start_point() {
        let h = window();
        let start = at("2026-07-06T00:00:00Z");
        // Exactly on the inclusive lower bound: the half-open `end > start` rule
        // alone would drop it, so it is special-cased.
        assert!(h.overlaps(start, start));
        assert!(h.overlaps(at("2026-07-09T12:00:00Z"), at("2026-07-09T12:00:00Z")));
        // Exactly on the exclusive upper bound, and outside: still excluded.
        let end = at("2026-07-13T00:00:00Z");
        assert!(!h.overlaps(end, end));
        assert!(!h.overlaps(at("2026-01-01T00:00:00Z"), at("2026-01-01T00:00:00Z")));
    }
}
