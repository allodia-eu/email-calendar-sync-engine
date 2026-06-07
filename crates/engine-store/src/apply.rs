//! The atomic apply batch and the precomputed derived rows it carries.
//!
//! `apply_sync_update` commits exactly one transaction for one scope, gated by a
//! lease token (`store-and-sync.md`). Everything it writes is bundled here so the
//! all-or-nothing set is self-documenting: normalized objects, precomputed FTS
//! and occurrence rows, pending-op reconciliations, and the next cursor.
//!
//! The store is mechanical — it never computes the derived rows. Recurrence
//! expansion and text extraction run in pure engine code *before* the call, which
//! keeps the write transaction short (no expansion under lock).

use engine_core::ids::ProviderKey;
use engine_core::sync::{SyncState, SyncUpdate};
use engine_core::time::UtcDateTime;
use engine_core::write::PendingOpId;
use serde::{Deserialize, Serialize};

use crate::outbox::PendingOpState;

/// An object the store can persist mechanically.
///
/// The store performs no normalization: it keys rows by the object's
/// [`ProviderKey`] and writes the serialized projection verbatim. Structured and
/// full-text projections are precomputed separately into [`DerivedWrite`]. The
/// normalized domain types (`Message`, `CalendarEvent`, `Mailbox`, `Calendar`)
/// implement this. The serialization bound is required at the `Store` method, not
/// here, so this trait stays minimal and serde-agnostic.
pub trait StorableObject {
    /// The provider key the store keys this object's rows by.
    fn provider_key(&self) -> &ProviderKey;
}

/// Field-tagged searchable text for one object — one logical FTS document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsField {
    /// The field name (e.g. `subject`, `body`; later `attachment:report.pdf`).
    pub name: String,
    /// The field's text.
    pub text: String,
}

impl FtsField {
    /// Creates a named full-text field.
    #[must_use]
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
        }
    }
}

/// A full-text row to upsert: the searchable text for one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FtsRow {
    /// The object this text belongs to.
    pub key: ProviderKey,
    /// Field-tagged text segments, mapped by the store onto its native FTS engine
    /// (SQLite FTS5 columns, Postgres `tsvector`). Open-ended so indexed
    /// attachment text — a later, server-side capability — can be added as
    /// further fields without a schema change.
    pub fields: Vec<FtsField>,
}

impl FtsRow {
    /// Creates a full-text row for an object key.
    #[must_use]
    pub fn new(key: ProviderKey, fields: Vec<FtsField>) -> Self {
        Self { key, fields }
    }
}

/// One materialized occurrence of a (possibly recurring) event, within the
/// rolling horizon.
///
/// Range queries use these rows, not the master event (`store-and-sync.md`).
/// `start`/`end` are resolved UTC instants for indexing; the floating/zoned
/// semantics stay on the master event. `recurrence_id` is set for an overridden
/// instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OccurrenceRow {
    /// The master event this occurrence expands from.
    pub event: ProviderKey,
    /// The occurrence start instant.
    pub start: UtcDateTime,
    /// The occurrence end instant (exclusive).
    pub end: UtcDateTime,
    /// The `RECURRENCE-ID` instant if this is an overridden instance.
    pub recurrence_id: Option<UtcDateTime>,
}

/// Precomputed derived rows for one scope.
///
/// Written atomically with the objects (inside [`ApplyBatch`]) or alone via
/// maintenance (horizon advance, tzdata change, on-demand body fetch). The store
/// writes these mechanically and never computes them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedWrite {
    /// Full-text rows to upsert.
    pub fts: Vec<FtsRow>,
    /// Expanded calendar occurrences to upsert.
    pub occurrences: Vec<OccurrenceRow>,
    /// Object keys whose derived rows (FTS and occurrences) must be removed —
    /// e.g. on tombstone, recurrence-rule change, or timezone-data change.
    pub removed: Vec<ProviderKey>,
}

impl DerivedWrite {
    /// An empty derived write (nothing to upsert or remove).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns `true` if there is nothing to write.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fts.is_empty() && self.occurrences.is_empty() && self.removed.is_empty()
    }
}

/// Counts from a successful apply, for diagnostics.
///
/// Idempotency is defined by *state* — a replayed batch leaves identical rows —
/// not by these counts, which simply report the work the apply performed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncApplied {
    /// Objects upserted.
    pub upserted: usize,
    /// Local rows tombstoned (snapshot reconciliation or explicit removals).
    pub tombstoned: usize,
    /// Pending-op reconciliations applied.
    pub reconciled: usize,
}

/// A planned match between an incoming synced object and an outstanding pending
/// op (e.g. by generated `Message-ID`), resolved inside the apply transaction.
///
/// The match is planned off-transaction by reading pending ops, so there is a
/// TOCTOU window. Inside the apply the store re-validates that the op is still in
/// `expected`; on mismatch it **skips** the reconciliation and stores the
/// incoming object normally, leaving duplicate suppression to presentation-layer
/// dedup (`store-and-sync.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReconciliation {
    /// The pending op being resolved.
    pub op: PendingOpId,
    /// The state the planner observed the op in; the apply skips if it no longer
    /// holds.
    pub expected: PendingOpState,
    /// The provider key the op resolves to (the now-synced object).
    pub resolves_to: ProviderKey,
}

impl PendingReconciliation {
    /// Plans a reconciliation of `op` (expected to be in `expected`) to the
    /// provider key it produced.
    #[must_use]
    pub fn new(op: PendingOpId, expected: PendingOpState, resolves_to: ProviderKey) -> Self {
        Self {
            op,
            expected,
            resolves_to,
        }
    }
}

/// The all-or-nothing set committed by `apply_sync_update` for one scope.
///
/// Bundled as one struct so the atomic set is self-documenting. Items 2–4 (the
/// derived rows and reconciliations) are precomputed by pure engine code before
/// the call.
#[derive(Debug)]
pub struct ApplyBatch<'a, T> {
    /// Provider-normalized objects for the scope, as a delta or a snapshot.
    pub update: &'a SyncUpdate<T>,
    /// Precomputed FTS and occurrence rows for the same objects.
    pub derived: &'a DerivedWrite,
    /// Pending-op reconciliations to resolve in the same transaction.
    pub reconcile: &'a [PendingReconciliation],
    /// The cursor to advance to on commit.
    pub next_state: &'a SyncState,
}

impl<'a, T> ApplyBatch<'a, T> {
    /// Assembles an apply batch from its parts.
    #[must_use]
    pub fn new(
        update: &'a SyncUpdate<T>,
        derived: &'a DerivedWrite,
        reconcile: &'a [PendingReconciliation],
        next_state: &'a SyncState,
    ) -> Self {
        Self {
            update,
            derived,
            reconcile,
            next_state,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn derived_write_emptiness_tracks_all_three_row_kinds() {
        let mut d = DerivedWrite::empty();
        assert!(d.is_empty());
        d.fts.push(FtsRow::new(
            key("m1"),
            vec![
                FtsField::new("subject", "hi"),
                FtsField::new("body", "there"),
            ],
        ));
        assert!(!d.is_empty());

        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<DerivedWrite>(&json).unwrap(), d);
    }

    #[test]
    fn occurrence_row_roundtrips_with_optional_override() {
        let occ = OccurrenceRow {
            event: key("evt-1"),
            start: "2026-03-01T09:00:00Z".parse().unwrap(),
            end: "2026-03-01T10:00:00Z".parse().unwrap(),
            recurrence_id: Some("2026-03-01T09:00:00Z".parse().unwrap()),
        };
        let json = serde_json::to_string(&occ).unwrap();
        assert_eq!(serde_json::from_str::<OccurrenceRow>(&json).unwrap(), occ);
    }

    #[test]
    fn reconciliation_carries_expected_state_and_target() {
        let rec =
            PendingReconciliation::new(PendingOpId::new(3), PendingOpState::InFlight, key("m9"));
        assert_eq!(rec.op, PendingOpId::new(3));
        assert_eq!(rec.expected, PendingOpState::InFlight);
        assert_eq!(rec.resolves_to, key("m9"));
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(
            serde_json::from_str::<PendingReconciliation>(&json).unwrap(),
            rec
        );
    }

    #[test]
    fn apply_batch_borrows_its_parts() {
        let update: SyncUpdate<String> = SyncUpdate::delta(vec!["a".to_owned()], vec![]);
        let derived = DerivedWrite::empty();
        let recs: Vec<PendingReconciliation> = Vec::new();
        let next = SyncState::new("cursor-2");
        let batch = ApplyBatch::new(&update, &derived, &recs, &next);
        assert!(batch.derived.is_empty());
        assert_eq!(batch.next_state.as_str(), "cursor-2");
        assert!(batch.reconcile.is_empty());
    }
}
