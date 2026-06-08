//! The atomic apply batch and the precomputed derived rows it carries.
//!
//! `apply_sync_update` commits exactly one transaction for one scope, gated by a
//! lease token (`store-and-sync.md`). Everything it writes is bundled here so the
//! all-or-nothing set is self-documenting: normalized objects, precomputed
//! full-text, structured-filter, and occurrence rows, pending-op reconciliations,
//! and the next cursor.
//!
//! The store is mechanical — it never computes the derived rows. Recurrence
//! expansion and text/structured extraction run in pure engine code *before* the
//! call: the full-text and structured rows come from
//! [`engine_core::search_index`] (re-exported below and assembled with
//! [`DerivedWrite::push_mail`]/[`DerivedWrite::push_event`]), and occurrence
//! expansion comes from the recurrence layer. This keeps the write transaction
//! short (no expansion under lock).

use core::fmt;

use engine_core::ids::ProviderKey;
use engine_core::search_index::{
    EventIndexRow, EventParticipantRow, EventProjection, MailAddressRow, MailIndexRow,
    MailProjection, MembershipRow,
};
use engine_core::sync::{SyncState, SyncUpdate};
use engine_core::time::UtcDateTime;
use engine_core::write::PendingOpId;
use serde::{Deserialize, Serialize};

use crate::outbox::PendingOpState;

// The full-text row types are defined in engine-core, beside the projection that
// produces them; they are re-exported here so the store's `DerivedWrite`
// vocabulary stays discoverable in one place.
pub use engine_core::search_index::{FtsField, FtsRow};

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

impl StorableObject for engine_core::mail::Message {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl StorableObject for engine_core::calendar::Event {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl StorableObject for engine_core::mail::Mailbox {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

impl StorableObject for engine_core::calendar::Calendar {
    fn provider_key(&self) -> &ProviderKey {
        self.id.key()
    }
}

/// The bundled IANA tzdata release an occurrence was expanded under.
///
/// Each materialized occurrence records this so a tzdata-version bump can find
/// and invalidate exactly the occurrences expanded under an older release, then
/// re-expand them (`calendar-semantics.md`); occurrences whose zones did not
/// change stay byte-stable. It is a non-key column — re-expansion updates it in
/// place. The value is the recurrence layer's bundled tzdb version (e.g.
/// `"2025b"`, from `jiff-tzdb`); the store treats it as opaque text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TzdataVersion(Box<str>);

impl TzdataVersion {
    /// Wraps a bundled-tzdb version string.
    #[must_use]
    pub fn new(version: impl Into<String>) -> Self {
        Self(version.into().into_boxed_str())
    }

    /// Returns the version string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TzdataVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One materialized occurrence of a (possibly recurring) event, within the
/// rolling horizon.
///
/// Range queries use these rows, not the master event (`store-and-sync.md`).
/// `start`/`end` are resolved UTC instants for indexing; the floating/zoned
/// semantics stay on the master event. `recurrence_id` is set for an overridden
/// instance. `tzdata_version` records the bundled tzdb release the resolution used
/// (`calendar-semantics.md`). Unlike the full-text and structured rows,
/// occurrences are **not** produced by [`engine_core::search_index`]: expanding
/// recurrence to UTC instants needs bundled tzdata and lives in the
/// recurrence/index layer, so this carrier type stays here.
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
    /// The bundled tzdata release this occurrence was expanded under.
    pub tzdata_version: TzdataVersion,
}

/// Precomputed derived rows for one scope.
///
/// Written atomically with the objects (inside [`ApplyBatch`]) or alone via
/// maintenance (horizon advance, tzdata change, on-demand body fetch). The store
/// writes these mechanically and never computes them. All rows are keyed by their
/// object's [`ProviderKey`], so replay is idempotent (upsert/replace, not append)
/// and [`Self::removed`] clears every derived kind for a key together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedWrite {
    /// Full-text rows to upsert.
    pub fts: Vec<FtsRow>,
    /// Expanded calendar occurrences to upsert.
    pub occurrences: Vec<OccurrenceRow>,
    /// Mail scalar-index rows to upsert.
    pub mail_index: Vec<MailIndexRow>,
    /// Mail address-junction rows to replace (per object).
    pub addresses: Vec<MailAddressRow>,
    /// Mailbox/keyword/calendar membership rows to replace (per object).
    pub memberships: Vec<MembershipRow>,
    /// Event scalar-index rows to upsert.
    pub event_index: Vec<EventIndexRow>,
    /// Event participant-junction rows to replace (per object).
    pub participants: Vec<EventParticipantRow>,
    /// Object keys whose derived rows (every kind above) must be removed — e.g. on
    /// tombstone, recurrence-rule change, or timezone-data change.
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
        self.fts.is_empty()
            && self.occurrences.is_empty()
            && self.mail_index.is_empty()
            && self.addresses.is_empty()
            && self.memberships.is_empty()
            && self.event_index.is_empty()
            && self.participants.is_empty()
            && self.removed.is_empty()
    }

    /// Adds the rows of a mail projection ([`engine_core::search_index::project_message`]):
    /// its full-text document, scalar row, address junctions, and memberships.
    pub fn push_mail(&mut self, projection: MailProjection) {
        let MailProjection {
            fts,
            index,
            addresses,
            memberships,
        } = projection;
        self.fts.push(fts);
        self.mail_index.push(index);
        self.addresses.extend(addresses);
        self.memberships.extend(memberships);
    }

    /// Adds the rows of an event projection ([`engine_core::search_index::project_event`]):
    /// its full-text document, scalar row, participant junctions, and memberships.
    /// Occurrence rows are added separately (they come from recurrence expansion).
    pub fn push_event(&mut self, projection: EventProjection) {
        let EventProjection {
            fts,
            index,
            participants,
            memberships,
        } = projection;
        self.fts.push(fts);
        self.event_index.push(index);
        self.participants.extend(participants);
        self.memberships.extend(memberships);
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
/// Bundled as one struct so the atomic set is self-documenting. The derived rows
/// and reconciliations are precomputed by pure engine code before the call.
#[derive(Debug)]
pub struct ApplyBatch<'a, T> {
    /// Provider-normalized objects for the scope, as a delta or a snapshot.
    pub update: &'a SyncUpdate<T>,
    /// Precomputed full-text, structured, and occurrence rows for the same objects.
    pub derived: &'a DerivedWrite,
    /// Pending-op reconciliations to resolve in the same transaction.
    pub reconcile: &'a [PendingReconciliation],
    /// The cursor to advance to on commit, or `None` to **leave the scope cursor
    /// unchanged**.
    ///
    /// `None` is for **incremental/streaming** applies: a fetch that spans many
    /// pages commits each page additively (objects + derived rows become visible)
    /// without yet marking the scope synced, then a final apply carries the real
    /// `Some(cursor)`. A crash mid-stream therefore leaves the prior cursor intact,
    /// so the next sync re-runs from it (idempotently) rather than skipping the
    /// un-applied pages.
    pub next_state: Option<&'a SyncState>,
}

impl<'a, T> ApplyBatch<'a, T> {
    /// Assembles an apply batch that advances the cursor to `next_state` on commit.
    #[must_use]
    pub fn new(
        update: &'a SyncUpdate<T>,
        derived: &'a DerivedWrite,
        reconcile: &'a [PendingReconciliation],
        next_state: &'a SyncState,
    ) -> Self {
        Self::with_cursor(update, derived, reconcile, Some(next_state))
    }

    /// Assembles an apply batch with an explicit cursor disposition: `Some(state)`
    /// advances the cursor, `None` leaves it unchanged (a streaming page).
    #[must_use]
    pub fn with_cursor(
        update: &'a SyncUpdate<T>,
        derived: &'a DerivedWrite,
        reconcile: &'a [PendingReconciliation],
        next_state: Option<&'a SyncState>,
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
    use engine_core::ids::{EventId, MailboxId, MessageId, Uid};
    use engine_core::mail::{EmailAddress, Message};
    use engine_core::membership::Memberships;
    use engine_core::search_index::{
        MembershipKind, OwnerAddresses, project_event, project_message,
    };
    use engine_core::time::{CalendarDateTime, LocalDateTime, TimeZoneId};

    fn key(value: &str) -> ProviderKey {
        ProviderKey::new(value).unwrap()
    }

    #[test]
    fn derived_write_emptiness_tracks_every_row_kind() {
        let mut d = DerivedWrite::empty();
        assert!(d.is_empty());
        d.fts
            .push(FtsRow::new(key("m1"), vec![FtsField::new("subject", "hi")]));
        assert!(!d.is_empty());

        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<DerivedWrite>(&json).unwrap(), d);
    }

    #[test]
    fn push_mail_flattens_a_projection_into_the_row_lists() {
        let mut msg = Message::new(
            MessageId::try_from("m1").unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        );
        msg.envelope.subject = Some("hello".into());
        msg.envelope.from = vec![EmailAddress::new("a@example.com")];

        let mut d = DerivedWrite::empty();
        d.push_mail(project_message(&msg));
        assert_eq!(d.fts.len(), 1);
        assert_eq!(d.mail_index.len(), 1);
        assert_eq!(d.addresses.len(), 1);
        assert_eq!(d.memberships.len(), 1);
        assert_eq!(d.memberships[0].kind, MembershipKind::Mailbox);
    }

    #[test]
    fn push_event_flattens_a_projection_into_the_row_lists() {
        let event = engine_core::calendar::Event::new(
            EventId::try_from("e1").unwrap(),
            Uid::new("uid-1").unwrap(),
            Memberships::of_one(engine_core::ids::CalendarId::try_from("work").unwrap()),
            CalendarDateTime::Zoned {
                local: LocalDateTime::new(2026, 6, 1, 9, 0, 0).unwrap(),
                zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            },
        );
        let mut d = DerivedWrite::empty();
        d.push_event(project_event(&event, &OwnerAddresses::default()));
        assert_eq!(d.event_index.len(), 1);
        assert_eq!(d.memberships.len(), 1);
        assert_eq!(d.memberships[0].kind, MembershipKind::Calendar);
    }

    #[test]
    fn container_objects_are_storable_by_their_id() {
        use engine_core::calendar::Calendar;
        use engine_core::ids::CalendarId;
        use engine_core::mail::Mailbox;

        let mailbox = Mailbox::new(MailboxId::try_from("inbox").unwrap(), "Inbox");
        assert_eq!(mailbox.provider_key().as_str(), "inbox");
        let calendar = Calendar::new(CalendarId::try_from("work").unwrap(), "Work");
        assert_eq!(calendar.provider_key().as_str(), "work");
    }

    #[test]
    fn occurrence_row_roundtrips_with_optional_override() {
        let occ = OccurrenceRow {
            event: key("evt-1"),
            start: "2026-03-01T09:00:00Z".parse().unwrap(),
            end: "2026-03-01T10:00:00Z".parse().unwrap(),
            recurrence_id: Some("2026-03-01T09:00:00Z".parse().unwrap()),
            tzdata_version: TzdataVersion::new("2025b"),
        };
        let json = serde_json::to_string(&occ).unwrap();
        assert_eq!(serde_json::from_str::<OccurrenceRow>(&json).unwrap(), occ);
        assert_eq!(occ.tzdata_version.as_str(), "2025b");
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
        assert_eq!(batch.next_state.unwrap().as_str(), "cursor-2");
        assert!(batch.reconcile.is_empty());

        // A streaming page leaves the cursor unchanged.
        let held = ApplyBatch::with_cursor(&update, &derived, &recs, None);
        assert!(held.next_state.is_none());
    }
}
