//! Recurrence sets: rules, exclusions, and per-instance overrides.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::RecurrenceRule;
use crate::patch::PatchObject;
use crate::time::LocalDateTime;

/// What a `recurrenceOverrides` entry does to one instance (RFC 8984 §4.3.5).
///
/// An override either removes the instance or patches it. The "an `excluded`
/// override MUST NOT patch any other property" rule is made structural: the
/// [`RecurrenceOverride::Excluded`] variant carries no patch at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RecurrenceOverride {
    /// The instance is excluded from the set (the `excluded: true` case;
    /// EXDATE-like).
    Excluded,
    /// The instance is added (if the recurrence rules did not already produce
    /// it; RDATE-like) and/or modified by this patch. An empty patch adds an
    /// unmodified extra instance.
    Patch(PatchObject),
}

/// The full recurrence specification of an event.
///
/// The instance set is, in order (RFC 8984 §4.3): the union of `rules` (with the
/// master start always included as the first instance), minus `excluded_rules`,
/// with `overrides` then applied by recurrence id. The override map is keyed by
/// the instance's original recurrence id, a wall-clock value in the event's
/// zone. Materializing the set is the expander's job, not this crate's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Recurrence {
    /// The recurrence rules whose union generates instances.
    pub rules: Vec<RecurrenceRule>,
    /// Rules whose instances are subtracted from the set.
    pub excluded_rules: Vec<RecurrenceRule>,
    /// Per-instance overrides, keyed by recurrence id (original start).
    pub overrides: BTreeMap<LocalDateTime, RecurrenceOverride>,
}

impl Recurrence {
    /// Creates a recurrence from a single rule, with no exclusions or overrides.
    #[must_use]
    pub fn from_rule(rule: RecurrenceRule) -> Self {
        Self {
            rules: vec![rule],
            excluded_rules: Vec::new(),
            overrides: BTreeMap::new(),
        }
    }

    /// Returns `true` if the given recurrence id is excluded by an override.
    #[must_use]
    pub fn is_excluded(&self, recurrence_id: &LocalDateTime) -> bool {
        matches!(
            self.overrides.get(recurrence_id),
            Some(RecurrenceOverride::Excluded)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::Frequency;

    fn recurrence_id(value: &str) -> LocalDateTime {
        value.parse().unwrap()
    }

    #[test]
    fn override_map_keyed_by_recurrence_id_roundtrips() {
        let mut rec = Recurrence::from_rule(RecurrenceRule::new(Frequency::Weekly));
        rec.overrides.insert(
            recurrence_id("2021-06-07T09:00:00"),
            RecurrenceOverride::Excluded,
        );
        rec.overrides.insert(
            recurrence_id("2021-06-14T09:00:00"),
            RecurrenceOverride::Patch(
                PatchObject::new([("title".to_owned(), serde_json::json!("Moved"))]).unwrap(),
            ),
        );

        assert!(rec.is_excluded(&recurrence_id("2021-06-07T09:00:00")));
        assert!(!rec.is_excluded(&recurrence_id("2021-06-14T09:00:00")));

        let json = serde_json::to_string(&rec).unwrap();
        let back: Recurrence = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn excluded_override_carries_no_patch() {
        // `Excluded` is a distinct variant, so it is impossible to attach a patch
        // to an exclusion — the RFC 8984 §4.3.5 rule is unrepresentable to break.
        let excluded = RecurrenceOverride::Excluded;
        assert!(matches!(excluded, RecurrenceOverride::Excluded));
    }
}
