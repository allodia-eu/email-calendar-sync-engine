//! Scheduling reconciliation keys: the [`InstanceKey`] that identifies a target
//! and the [`Revision`] that orders messages for it (RFC 5546 §2.1.5).

use serde::{Deserialize, Serialize};

use crate::ids::Uid;
use crate::time::{CalendarDateTime, UtcDateTime};

/// Identifies a single scheduling target: a whole series, or one instance of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InstanceKey {
    /// The event's cross-system uid.
    pub uid: Uid,
    /// The recurrence id of a single targeted instance, or `None` for the whole
    /// series/master.
    pub recurrence_id: Option<CalendarDateTime>,
}

impl InstanceKey {
    /// A key targeting the whole series (no recurrence id).
    #[must_use]
    pub fn series(uid: Uid) -> Self {
        Self {
            uid,
            recurrence_id: None,
        }
    }

    /// A key targeting a single instance.
    #[must_use]
    pub fn instance(uid: Uid, recurrence_id: CalendarDateTime) -> Self {
        Self {
            uid,
            recurrence_id: Some(recurrence_id),
        }
    }

    /// Returns `true` if this key targets the whole series rather than one
    /// instance.
    #[must_use]
    pub fn is_series(&self) -> bool {
        self.recurrence_id.is_none()
    }
}

/// The revision of a scheduling message for one [`InstanceKey`]: its `SEQUENCE`
/// and `DTSTAMP`.
///
/// `Ord` compares `sequence` first, then `dtstamp`, so the maximum revision is
/// the winner of iTIP message sequencing (RFC 5546 §2.1.5). The field order is
/// load-bearing for the derived ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision {
    /// The iTIP `SEQUENCE` (higher obsoletes lower).
    pub sequence: u32,
    /// The `DTSTAMP`, the tie-breaker when sequences are equal.
    pub dtstamp: UtcDateTime,
}

impl Revision {
    /// Creates a revision.
    #[must_use]
    pub fn new(sequence: u32, dtstamp: UtcDateTime) -> Self {
        Self { sequence, dtstamp }
    }

    /// Returns `true` if `self` supersedes `current` — a strictly higher
    /// `SEQUENCE`, or an equal `SEQUENCE` with a later `DTSTAMP`. An equal
    /// revision does **not** supersede (idempotent re-delivery is ignored).
    #[must_use]
    pub fn supersedes(&self, current: &Revision) -> bool {
        self > current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dtstamp(s: &str) -> UtcDateTime {
        s.parse().unwrap()
    }

    #[test]
    fn higher_sequence_supersedes() {
        let v1 = Revision::new(1, dtstamp("2021-01-01T00:00:00Z"));
        let v2 = Revision::new(2, dtstamp("2020-01-01T00:00:00Z")); // older stamp, higher seq
        assert!(v2.supersedes(&v1));
        assert!(!v1.supersedes(&v2)); // a stale lower-SEQUENCE message loses
    }

    #[test]
    fn equal_sequence_breaks_on_dtstamp() {
        let earlier = Revision::new(3, dtstamp("2021-01-01T09:00:00Z"));
        let later = Revision::new(3, dtstamp("2021-01-01T10:00:00Z"));
        assert!(later.supersedes(&earlier));
        // Idempotent re-delivery (identical revision) does not supersede.
        assert!(!later.supersedes(&later.clone()));
    }

    #[test]
    fn instance_key_distinguishes_series_from_instance() {
        let uid = Uid::new("uid-1").unwrap();
        let series = InstanceKey::series(uid.clone());
        let instance = InstanceKey::instance(
            uid,
            CalendarDateTime::Floating("2021-06-07T09:00:00".parse().unwrap()),
        );
        assert!(series.is_series());
        assert!(!instance.is_series());
        assert_ne!(series, instance);
    }
}
