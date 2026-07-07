//! Calendar-write outbox-driver tests: enqueue-then-put/delete success recording,
//! a 412 conflict recorded without a blind retry, distinct idempotency keys letting
//! two edits of one resource both run, and durable write/deletion payload
//! round-trip. Uses the shared fakes and helpers from the parent module via
//! `use super::*`.

use super::*;

fn event_write(href: &str, uid: &str) -> EventWrite {
    EventWrite::create(
        EventId::try_from(href).unwrap(),
        Uid::new(uid).unwrap(),
        RawIcal::new(format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR"
        )),
    )
}

#[tokio::test]
async fn write_calendar_event_enqueues_then_puts_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let write = event_write("/cal/default/evt-1.ics", "evt-1@test.local");

    let outcome = write_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "put:evt-1:rev1",
        &write,
    )
    .await
    .unwrap();

    assert_eq!(outcome.event_key.as_str(), "/cal/default/evt-1.ics");
    assert_eq!(outcome.uid.as_str(), "evt-1@test.local");
    assert_eq!(outcome.etag, Some(ETag::new("\"put-v1\"")));
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn write_calendar_event_records_conflict_without_blind_retry() {
    // A 412 precondition failure is recorded Failed (class Conflict) and returned —
    // the caller refetches and merges, the outbox does not blind-retry.
    let provider = FakeMail::new(vec![], vec![]).conflicting_writes();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let write = event_write("/cal/default/evt-2.ics", "evt-2@test.local");

    let err = write_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "put:evt-2:rev1",
        &write,
    )
    .await
    .unwrap_err();
    match err {
        crate::SyncError::Provider(e) => {
            assert_eq!(e.class(), engine_core::error::FailureClass::Conflict);
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    // Recover the op id via an idempotent re-enqueue; it was recorded Failed.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("put:evt-2:rev1").unwrap(),
                ResourceKey::new("caldav:/cal/default/evt-2.ics").unwrap(),
                serde_json::Value::Null,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Failed)
    );
}

#[tokio::test]
async fn delete_calendar_event_enqueues_then_deletes_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let deletion = EventDeletion::if_match(
        EventId::try_from("/cal/default/evt-3.ics").unwrap(),
        ETag::new("\"v1\""),
    );

    let op = delete_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "delete:evt-3",
        &deletion,
    )
    .await
    .unwrap();
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn distinct_idempotency_keys_let_two_edits_of_one_resource_both_run() {
    // The store dedups enqueue by (account, idempotency_key) across every op state,
    // so two successive edits of ONE href must carry distinct keys to both run —
    // the reason the key is a caller-supplied argument, not derived from the href.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let href = "/cal/default/evt-4.ics";

    let first = write_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "put:evt-4:rev1",
        &event_write(href, "evt-4@test.local"),
    )
    .await
    .unwrap();
    let second = write_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "put:evt-4:rev2",
        &event_write(href, "evt-4@test.local"),
    )
    .await
    .unwrap();

    // Two distinct durable ops, both terminal-success — the second edit was not
    // collapsed into the first.
    assert_ne!(first.op, second.op);
    assert_eq!(
        store.pending_op_state(second.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[test]
fn event_write_and_deletion_round_trip_through_durable_payloads() {
    // The outbox stores the write/deletion as JSON payloads; they must survive
    // intact for a recovery worker to re-apply them.
    let write = event_write("/cal/default/evt-5.ics", "evt-5@test.local");
    let payload = serde_json::to_value(&write).unwrap();
    assert_eq!(
        serde_json::from_value::<EventWrite>(payload).unwrap(),
        write
    );

    let deletion = EventDeletion::if_match(
        EventId::try_from("/cal/default/evt-5.ics").unwrap(),
        ETag::new("\"v1\""),
    );
    let payload = serde_json::to_value(&deletion).unwrap();
    assert_eq!(
        serde_json::from_value::<EventDeletion>(payload).unwrap(),
        deletion
    );
}
