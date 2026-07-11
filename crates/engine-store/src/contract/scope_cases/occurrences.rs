//! Occurrence range-read cases: the windowed read a calendar grid pages over.
//!
//! `event_occurrence` is the only derived kind a host queries by *range* rather
//! than by key (`store-and-sync.md`), so these cases pin the window semantics every
//! backend must reproduce: half-open at both ends, straddling rows included, ordering
//! deterministic.

use engine_core::{
    sync::{SyncState, SyncUpdate},
    time::{Horizon, UtcDateTime},
};

use super::super::{TestObject, acct, event_scope, lease_request, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite, OccurrenceRow, TzdataVersion},
    lease::ManualClock,
    store::{Store, StoreRead},
};

fn at(raw: &str) -> UtcDateTime {
    raw.parse().expect("valid instant")
}

fn occurrence(event: &str, start: &str, end: &str) -> OccurrenceRow {
    OccurrenceRow {
        event: pk(event),
        start: at(start),
        end: at(end),
        recurrence_id: None,
        tzdata_version: TzdataVersion::new("2026a"),
    }
}

/// `scope_occurrences` returns exactly the occurrences overlapping the window, in
/// ascending `(start, end, event)` order.
///
/// The window is **half-open at both ends**: a row that merely touches an edge —
/// ending exactly when the window opens, or starting exactly when it closes — does
/// not overlap, so a week grid never double-renders a midnight-boundary event into
/// two adjacent weeks. A row that straddles an edge, or spans the whole window,
/// does overlap: a multi-day event must appear on every day it covers, not only the
/// day it started. Ordering is specified (not "unspecified — callers sort", as for
/// the mail index) because a host lays these rows out geometrically, and an unstable
/// order would place an overlapping event in a different column each read.
pub(in crate::contract) async fn scope_occurrences_reads_the_overlapping_window<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-occ");
    let scope = event_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    // The window is the week of Mon 2026-07-06 .. Mon 2026-07-13.
    let week = Horizon::new(at("2026-07-06T00:00:00Z"), at("2026-07-13T00:00:00Z")).unwrap();

    let mut derived = DerivedWrite::empty();
    // Inside the window: a recurring event's two instances, and a one-off between them.
    derived.occurrences.push(occurrence(
        "weekly",
        "2026-07-06T09:00:00Z",
        "2026-07-06T09:15:00Z",
    ));
    derived.occurrences.push(occurrence(
        "oneoff",
        "2026-07-08T14:00:00Z",
        "2026-07-08T15:00:00Z",
    ));
    derived.occurrences.push(occurrence(
        "weekly",
        "2026-07-13T09:00:00Z",
        "2026-07-13T09:15:00Z",
    ));
    // Straddling each edge: a conference starting the Sunday before and ending inside,
    // and an event starting inside and running past the window's end.
    derived.occurrences.push(occurrence(
        "straddle-start",
        "2026-07-05T20:00:00Z",
        "2026-07-06T02:00:00Z",
    ));
    derived.occurrences.push(occurrence(
        "straddle-end",
        "2026-07-12T22:00:00Z",
        "2026-07-14T10:00:00Z",
    ));
    // Spanning the whole window (a fortnight-long all-day booking): it covers every
    // day in view, so it must come back even though it neither starts nor ends here.
    derived.occurrences.push(occurrence(
        "spanning",
        "2026-06-29T00:00:00Z",
        "2026-07-20T00:00:00Z",
    ));
    // Touching an edge only: `ends-at-open` ends exactly when the window opens and
    // `starts-at-close` starts exactly when it closes. Neither overlaps.
    derived.occurrences.push(occurrence(
        "ends-at-open",
        "2026-07-05T22:00:00Z",
        "2026-07-06T00:00:00Z",
    ));
    derived.occurrences.push(occurrence(
        "starts-at-close",
        "2026-07-13T00:00:00Z",
        "2026-07-13T01:00:00Z",
    ));
    // Wholly outside.
    derived.occurrences.push(occurrence(
        "long-past",
        "2026-01-05T09:00:00Z",
        "2026-01-05T10:00:00Z",
    ));

    let update = SyncUpdate::delta(
        vec![
            TestObject::new("weekly", "W"),
            TestObject::new("oneoff", "O"),
            TestObject::new("straddle-start", "SS"),
            TestObject::new("straddle-end", "SE"),
            TestObject::new("spanning", "SP"),
            TestObject::new("ends-at-open", "EO"),
            TestObject::new("starts-at-close", "SC"),
            TestObject::new("long-past", "LP"),
        ],
        vec![],
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("occ-1")),
        )
        .await
        .unwrap();

    let rows = store.scope_occurrences(&scope, week).await.unwrap();
    let got: Vec<(&str, UtcDateTime)> = rows
        .iter()
        .map(|row| (row.event.as_str(), row.start))
        .collect();
    assert_eq!(
        got,
        vec![
            ("spanning", at("2026-06-29T00:00:00Z")),
            ("straddle-start", at("2026-07-05T20:00:00Z")),
            ("weekly", at("2026-07-06T09:00:00Z")),
            ("oneoff", at("2026-07-08T14:00:00Z")),
            ("straddle-end", at("2026-07-12T22:00:00Z")),
        ],
        "the window is half-open at both ends, straddling and spanning rows are in, \
         and the order is ascending by start"
    );
    // The recurrence's second instant is the window's exclusive upper bound, so it
    // belongs to the *next* page, not this one — a grid must not render it twice.
    assert!(
        !rows
            .iter()
            .any(|row| row.start == at("2026-07-13T09:00:00Z"))
    );
}

/// An occurrence's `recurrence_id` (an overridden instance) round-trips the range
/// read, and tombstoning the master event clears every occurrence it expanded to —
/// so a deleted series leaves no orphan rows behind in a window that already
/// contained them.
pub(in crate::contract) async fn scope_occurrences_keep_overrides_and_drop_with_the_event<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-occ-drop");
    let scope = event_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    let week = Horizon::new(at("2026-07-06T00:00:00Z"), at("2026-07-13T00:00:00Z")).unwrap();

    // A series with one plain instance and one *moved* instance: the override keeps
    // the original instant as its `recurrence_id` while starting somewhere else.
    let mut moved = occurrence("series", "2026-07-09T14:00:00Z", "2026-07-09T15:00:00Z");
    moved.recurrence_id = Some(at("2026-07-09T09:00:00Z"));
    let mut derived = DerivedWrite::empty();
    derived.occurrences.push(occurrence(
        "series",
        "2026-07-07T09:00:00Z",
        "2026-07-07T09:30:00Z",
    ));
    derived.occurrences.push(moved);
    // A second, unrelated event that must survive the first one's tombstone.
    derived.occurrences.push(occurrence(
        "other",
        "2026-07-10T11:00:00Z",
        "2026-07-10T12:00:00Z",
    ));

    let update = SyncUpdate::delta(
        vec![
            TestObject::new("series", "S"),
            TestObject::new("other", "O"),
        ],
        vec![],
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("occ-2")),
        )
        .await
        .unwrap();

    let rows = store.scope_occurrences(&scope, week).await.unwrap();
    assert_eq!(rows.len(), 3);
    // The moved instance keeps both instants: where it now is, and which one it replaced.
    let override_row = rows
        .iter()
        .find(|row| row.recurrence_id.is_some())
        .expect("the moved instance round-trips its recurrence id");
    assert_eq!(override_row.event, pk("series"));
    assert_eq!(override_row.start, at("2026-07-09T14:00:00Z"));
    assert_eq!(override_row.recurrence_id, Some(at("2026-07-09T09:00:00Z")));

    // Tombstoning the series clears *both* of its occurrences, and only its own.
    let drop_series: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![pk("series")]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(
                &drop_series,
                &DerivedWrite::empty(),
                &[],
                &SyncState::new("occ-3"),
            ),
        )
        .await
        .unwrap();

    let rows = store.scope_occurrences(&scope, week).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.event.as_str())
            .collect::<Vec<_>>(),
        vec!["other"],
        "a tombstoned series takes every occurrence it expanded to with it"
    );

    // A scope the store has never seen reads back empty rather than erroring.
    assert!(
        store
            .scope_occurrences(&event_scope(&acct("acct-occ-none")), week)
            .await
            .unwrap()
            .is_empty()
    );
}
