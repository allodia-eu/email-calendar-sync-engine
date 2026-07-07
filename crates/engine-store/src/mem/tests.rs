//! Unit tests for the reference store's derived-row and lease helpers.

use super::*;
use crate::{
    apply::{FtsRow, TzdataVersion},
    lease::{ManualClock, WorkerId},
};

fn key(value: &str) -> ProviderKey {
    ProviderKey::new(value).unwrap()
}

#[test]
fn expiry_after_advances_then_overflows_at_end_of_time() {
    let req = LeaseRequest::new(WorkerId::new("w"), core::time::Duration::from_secs(30));
    let early: UtcDateTime = "2026-01-01T00:00:00Z".parse().unwrap();
    assert!(expiry_after(early, &req).is_ok());

    // Past the end of representable time, expiry overflows to a backend error.
    let end_of_time: UtcDateTime = "9999-12-31T23:59:59Z".parse().unwrap();
    assert_eq!(
        expiry_after(end_of_time, &req),
        Err(StoreError::Backend("lease ttl overflow".to_owned()))
    );
}

#[test]
fn apply_derived_upserts_then_removes_fts_and_occurrences() {
    let mut cell = ScopeCell::new();
    let mut derived = DerivedWrite::empty();
    derived.fts.push(FtsRow::new(
        key("e1"),
        vec![FtsField::new("summary", "standup")],
    ));
    derived.occurrences.push(OccurrenceRow {
        event: key("e1"),
        start: "2026-03-01T09:00:00Z".parse().unwrap(),
        end: "2026-03-01T09:15:00Z".parse().unwrap(),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    });
    cell.apply_derived(&derived);
    assert!(cell.fts.contains_key(&key("e1")));
    assert_eq!(cell.occurrences.get(&key("e1")).map(Vec::len), Some(1));

    // A removal clears both the FTS and occurrence rows for the key.
    let mut removal = DerivedWrite::empty();
    removal.removed.push(key("e1"));
    cell.apply_derived(&removal);
    assert!(!cell.fts.contains_key(&key("e1")));
    assert!(!cell.occurrences.contains_key(&key("e1")));
}

#[test]
fn re_expansion_batch_clears_then_writes_in_one_pass() {
    // A tzdata-bump re-expansion arrives as one batch that both removes an
    // event's stale occurrences and writes the fresh ones. `removed` must be
    // processed first, or the clear would wipe the fresh rows.
    let mut cell = ScopeCell::new();
    let mut stale = DerivedWrite::empty();
    stale.occurrences.push(OccurrenceRow {
        event: key("e1"),
        start: "2026-03-01T09:00:00Z".parse().unwrap(),
        end: "2026-03-01T09:15:00Z".parse().unwrap(),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025a"),
    });
    cell.apply_derived(&stale);

    let mut re_expand = DerivedWrite::empty();
    re_expand.removed.push(key("e1"));
    re_expand.occurrences.push(OccurrenceRow {
        event: key("e1"),
        start: "2026-03-01T09:00:00Z".parse().unwrap(),
        end: "2026-03-01T09:15:00Z".parse().unwrap(),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2025b"),
    });
    cell.apply_derived(&re_expand);

    let occ = cell.occurrences.get(&key("e1")).unwrap();
    assert_eq!(occ.len(), 1);
    assert_eq!(occ[0].tzdata_version.as_str(), "2025b");
}

#[test]
fn mem_store_debug_is_redacted() {
    let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap());
    let store = MemStore::new(clock);
    assert!(format!("{store:?}").contains("MemStore"));
}
