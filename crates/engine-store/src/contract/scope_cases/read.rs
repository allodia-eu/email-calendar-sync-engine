//! Read-surface cases: structured index rows, account scope enumeration, and
//! the mail-index and batch-object scans.

use engine_core::{
    calendar::ParticipationStatus,
    ids::ThreadId,
    search_index::{
        AddressField, EventIndexRow, EventParticipantRow, MailAddressRow, MailIndexRow,
        MembershipKind, MembershipRow, ParticipantField,
    },
    sync::{SyncState, SyncUpdate},
};

use super::super::{TestObject, acct, email_scope, lease_request, mailbox_scope, pk};
use crate::{
    apply::{ApplyBatch, DerivedWrite, FtsField, FtsRow},
    lease::ManualClock,
    store::{IndexRowCounts, Store, StoreRead},
};

/// The mixed mail+event derived-row fixture the structured-index case applies:
/// an FTS doc, a mail scalar row, two addresses, a mailbox membership (key `m1`),
/// plus an event scalar row with my RSVP and two participants (key `e1`).
fn structured_index_fixture() -> DerivedWrite {
    let mail = pk("m1");
    let event = pk("e1");
    let mut derived = DerivedWrite::empty();
    derived.fts.push(FtsRow::new(
        mail.clone(),
        vec![FtsField::new("subject", "hello")],
    ));
    derived.mail_index.push(MailIndexRow {
        key: mail.clone(),
        date_utc: Some("2026-01-01T00:00:00Z".parse().unwrap()),
        has_attachment: true,
        thread_id: None,
    });
    derived.addresses.push(MailAddressRow {
        key: mail.clone(),
        field: AddressField::From,
        addr: "alice@example.com".into(),
        name: Some("Alice".into()),
    });
    derived.addresses.push(MailAddressRow {
        key: mail.clone(),
        field: AddressField::To,
        addr: "bob@example.com".into(),
        name: None,
    });
    derived.memberships.push(MembershipRow {
        key: mail,
        kind: MembershipKind::Mailbox,
        value: "inbox".into(),
    });
    derived.event_index.push(EventIndexRow {
        key: event.clone(),
        has_conference: true,
        my_partstat: Some(ParticipationStatus::Accepted),
    });
    derived.participants.push(EventParticipantRow {
        key: event.clone(),
        field: ParticipantField::Organizer,
        addr: "me@example.com".into(),
        partstat: ParticipationStatus::Accepted,
    });
    derived.participants.push(EventParticipantRow {
        key: event,
        field: ParticipantField::Attendee,
        addr: "guest@example.com".into(),
        partstat: ParticipationStatus::NeedsAction,
    });
    derived
}

/// Structured index rows (scalars + junctions) commit with the object, **replace**
/// on replay (no duplication), and clear together when the key's derived rows are
/// removed. Every backend stores them identically — verified through
/// [`StoreRead::index_row_counts`], so the contract holds the SQLite executor's
/// inputs to the same shape as the reference store.
pub(in crate::contract) async fn structured_index_rows_replace_and_clear<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-index");
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    let mail = pk("m1");
    let event = pk("e1");
    let derived = structured_index_fixture();

    let update = SyncUpdate::delta(vec![TestObject::new("m1", "x")], vec![]);
    let cursor = SyncState::new("idx-1");
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &cursor),
        )
        .await
        .unwrap();

    let mail_counts = IndexRowCounts {
        fts: 1,
        mail_index: 1,
        addresses: 2,
        memberships: 1,
        ..IndexRowCounts::default()
    };
    let event_counts = IndexRowCounts {
        event_index: 1,
        participants: 2,
        ..IndexRowCounts::default()
    };
    assert_eq!(
        store.index_row_counts(&scope, &mail).await.unwrap(),
        mail_counts
    );
    assert_eq!(
        store.index_row_counts(&scope, &event).await.unwrap(),
        event_counts
    );

    // Replay the identical batch: structured rows replace per object, so the
    // junction counts do not grow.
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &cursor),
        )
        .await
        .unwrap();
    assert_eq!(
        store.index_row_counts(&scope, &mail).await.unwrap(),
        mail_counts
    );
    assert_eq!(
        store.index_row_counts(&scope, &event).await.unwrap(),
        event_counts
    );

    // Removing the keys' derived rows (e.g. re-index) clears every kind together.
    let mut clear = DerivedWrite::empty();
    clear.removed.push(mail.clone());
    clear.removed.push(event.clone());
    store.apply_maintenance(&claim.lease, &clear).await.unwrap();
    assert!(
        store
            .index_row_counts(&scope, &mail)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .index_row_counts(&scope, &event)
            .await
            .unwrap()
            .is_empty()
    );
}

/// `account_scopes` lists exactly the scopes a store has claimed for an account —
/// across data types, in ascending `SyncScope` order — and nothing from another
/// account, so a per-account search enumerates them instead of hard-coding which
/// scopes a provider uses. An account the store has never seen has none.
pub(in crate::contract) async fn account_scopes_enumerates_an_accounts_scopes<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let a = acct("acct-enum-a");
    let b = acct("acct-enum-b");
    // Claiming a scope registers it; claim two data types for A and one for B.
    for scope in [mailbox_scope(&a), email_scope(&a)] {
        store
            .claim_sync_scope(a.clone(), &scope, lease_request("worker", 300))
            .await
            .unwrap();
    }
    store
        .claim_sync_scope(b.clone(), &email_scope(&b), lease_request("worker", 300))
        .await
        .unwrap();

    // A's scopes only, in ascending order (the suite asserts the exact ordered set).
    let mut expected = vec![email_scope(&a), mailbox_scope(&a)];
    expected.sort();
    assert_eq!(store.account_scopes(a).await.unwrap(), expected);
    assert_eq!(
        store.account_scopes(b).await.unwrap(),
        vec![email_scope(&acct("acct-enum-b"))]
    );
    // An account the store has never seen has no scopes.
    assert!(
        store
            .account_scopes(acct("acct-enum-none"))
            .await
            .unwrap()
            .is_empty()
    );
}

/// `scope_mail_index` returns each live mail object's `(key, date, thread)` — the scalar
/// sort/group keys backing the windowed and threaded reads — with dates and thread ids
/// intact, undated/unthreaded messages reported as `None`, and a tombstoned object excluded
/// (its index row is cleared with it).
pub(in crate::contract) async fn scope_mail_index_reports_dates_threads_and_excludes_tombstones<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-mailidx");
    let scope = email_scope(&account);
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    // Three messages: two share a thread with distinct dates, one is undated + unthreaded.
    let thread = ThreadId::try_from("t-shared").unwrap();
    let mut derived = DerivedWrite::empty();
    derived.mail_index.push(MailIndexRow {
        key: pk("m1"),
        date_utc: Some("2026-01-03T00:00:00Z".parse().unwrap()),
        has_attachment: false,
        thread_id: Some(thread.clone()),
    });
    derived.mail_index.push(MailIndexRow {
        key: pk("m2"),
        date_utc: Some("2026-01-01T00:00:00Z".parse().unwrap()),
        has_attachment: false,
        thread_id: Some(thread.clone()),
    });
    derived.mail_index.push(MailIndexRow {
        key: pk("m3"),
        date_utc: None,
        has_attachment: false,
        thread_id: None,
    });
    let update = SyncUpdate::delta(
        vec![
            TestObject::new("m1", "A"),
            TestObject::new("m2", "B"),
            TestObject::new("m3", "C"),
        ],
        vec![],
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("mi-1")),
        )
        .await
        .unwrap();

    let mut entries = store.scope_mail_index(&scope).await.unwrap();
    entries.sort_by(|a, b| a.0.cmp(&b.0)); // order is unspecified; sort by key to assert.
    assert_eq!(
        entries,
        vec![
            (
                pk("m1"),
                Some("2026-01-03T00:00:00Z".parse().unwrap()),
                Some(thread.clone()),
            ),
            (
                pk("m2"),
                Some("2026-01-01T00:00:00Z".parse().unwrap()),
                Some(thread.clone()),
            ),
            (pk("m3"), None, None),
        ]
    );

    // Tombstoning m2 clears its index row too, so it drops out of the scan.
    let drop_m2: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![pk("m2")]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(
                &drop_m2,
                &DerivedWrite::empty(),
                &[],
                &SyncState::new("mi-2"),
            ),
        )
        .await
        .unwrap();
    let keys: Vec<_> = store
        .scope_mail_index(&scope)
        .await
        .unwrap()
        .into_iter()
        .map(|(key, ..)| key)
        .collect();
    assert_eq!(keys.len(), 2);
    assert!(!keys.contains(&pk("m2")));
}

/// `scope_objects` batch-reads a scope's live objects as `(key, payload)` pairs in key
/// order — the read backing per-account views — matching `object_payload` per key and
/// excluding tombstoned objects.
pub(in crate::contract) async fn scope_objects_batch_reads_live_objects<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-objects");
    let scope = email_scope(&account);
    let derived = DerivedWrite::empty();
    let claim = store
        .claim_sync_scope(account.clone(), &scope, lease_request("worker", 300))
        .await
        .unwrap();

    let update = SyncUpdate::delta(
        vec![
            TestObject::new("a", "A"),
            TestObject::new("b", "B"),
            TestObject::new("c", "C"),
        ],
        vec![],
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&update, &derived, &[], &SyncState::new("c1")),
        )
        .await
        .unwrap();
    // Drop `b`: a tombstoned object must not appear in the batch read.
    let drop_b: SyncUpdate<TestObject> = SyncUpdate::delta(vec![], vec![pk("b")]);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&drop_b, &derived, &[], &SyncState::new("c2")),
        )
        .await
        .unwrap();

    // The two live objects, in ascending key order (so a multi-object sort runs).
    let objects = store.scope_objects(&scope).await.unwrap();
    assert_eq!(
        objects
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>(),
        vec![pk("a"), pk("c")]
    );
    // The batched payload matches the single-key read.
    assert_eq!(
        Some(&objects[0].1),
        store
            .object_payload(&scope, &pk("a"))
            .await
            .unwrap()
            .as_ref()
    );
    // A scope the store has never seen reads back empty.
    assert!(
        store
            .scope_objects(&email_scope(&acct("acct-objects-none")))
            .await
            .unwrap()
            .is_empty()
    );
}
