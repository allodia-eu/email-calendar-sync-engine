//! Calendar-write outbox-driver tests: enqueue-then-create/patch/delete success recording,
//! a guard failure recorded without a blind retry, distinct idempotency keys letting two
//! edits of one event both run, and the durable payload round-trip. Uses the shared fakes
//! and helpers from the parent module via `use super::*`.
//!
//! The drivers are provider-neutral, and these tests are written the way a host would call
//! them: state the intent, never assemble a protocol payload.

use super::*;

fn calendar() -> CalendarId {
    CalendarId::new(ProviderKey::new("/cal/default/").unwrap())
}

fn stamp() -> engine_core::time::UtcDateTime {
    "2026-07-14T10:00:00Z".parse().unwrap()
}

fn at(hour: u8) -> CalendarDateTime {
    CalendarDateTime::utc(
        format!("2026-08-01T{hour:02}:00:00")
            .parse::<LocalDateTime>()
            .unwrap(),
    )
}

fn draft(uid: &str) -> EventDraft {
    EventDraft::new(
        calendar(),
        Uid::new(uid).unwrap(),
        "Sprint planning",
        at(9),
        at(10),
        stamp(),
    )
}

/// A stored event as the store hands it back: the raw it was synced with, and the revision
/// it was read at.
fn stored(href: &str, uid: &str) -> Event {
    let mut event = Event::new(
        EventId::try_from(href).unwrap(),
        Uid::new(uid).unwrap(),
        Memberships::of_one(calendar()),
        at(9),
    );
    event.raw_ical = Some(RawIcal::new(format!(
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR"
    )));
    event.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
    event
}

#[tokio::test]
async fn create_calendar_event_enqueues_then_writes_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = create_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "create:evt-1",
        &draft("evt-1@test.local"),
    )
    .await
    .unwrap();

    // The caller learns the id the create *resolved to* — it never minted one.
    assert_eq!(outcome.event.as_str(), "/cal/evt-1@test.local.ics");
    assert_eq!(outcome.uid.as_str(), "evt-1@test.local");
    assert_eq!(outcome.revisions.etag, Some(ETag::new("\"put-v1\"")));
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn patch_calendar_event_enqueues_then_writes_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-2.ics", "evt-2@test.local");

    let outcome = patch_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "patch:evt-2:rev1",
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    )
    .await
    .unwrap();

    assert_eq!(outcome.event.as_str(), "/cal/default/evt-2.ics");
    assert_eq!(outcome.revisions.etag, Some(ETag::new("\"put-v1\"")));
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_failed_guard_is_recorded_without_a_blind_retry() {
    // A stale revision — a CalDAV `412`, a JMAP `stateMismatch` — is recorded Failed with a
    // Conflict class and returned. The caller re-syncs and re-applies the edit to the fresh
    // copy; the outbox never blind-retries a write whose base has moved.
    let provider = FakeMail::new(vec![], vec![]).conflicting_writes();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-3.ics", "evt-3@test.local");

    let err = patch_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "patch:evt-3:rev1",
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed"),
    )
    .await
    .unwrap_err();
    match err {
        crate::SyncError::Provider(e) => {
            assert_eq!(e.class(), engine_core::error::FailureClass::Conflict);
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    // Recover the op id via an idempotent re-enqueue; it was recorded Failed. The op is
    // serialized on the event's UID, which is the identity that exists on every transport.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("patch:evt-3:rev1").unwrap(),
                ResourceKey::new("event:evt-3@test.local").unwrap(),
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
    let base = stored("/cal/default/evt-4.ics", "evt-4@test.local");

    let op = delete_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "delete:evt-4",
        &EventDeletion::of(&base),
    )
    .await
    .unwrap();
    assert_eq!(
        store.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_failed_delete_is_recorded_too_not_just_a_failed_edit() {
    // The delete path records its own failure — a guarded delete whose revision the server
    // has superseded is a Conflict, and the durable op must say so rather than vanishing.
    let provider = FakeMail::new(vec![], vec![]).conflicting_writes();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-8.ics", "evt-8@test.local");

    let err = delete_calendar_event(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "delete:evt-8",
        &EventDeletion::of(&base),
    )
    .await
    .unwrap_err();
    match err {
        crate::SyncError::Provider(e) => {
            assert_eq!(e.class(), engine_core::error::FailureClass::Conflict);
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("delete:evt-8").unwrap(),
                ResourceKey::new("event:evt-8@test.local").unwrap(),
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
async fn a_document_write_rides_the_same_outbox() {
    // The iMIP RSVP path: the caller assembled the bytes itself.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-5.ics", "evt-5@test.local");

    let outcome = put_calendar_document(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "rsvp:evt-5:accept",
        &EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR")),
    )
    .await
    .unwrap();
    assert_eq!(outcome.event.as_str(), "/cal/default/evt-5.ics");
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn distinct_idempotency_keys_let_two_edits_of_one_event_both_run() {
    // The store dedups enqueue by (account, idempotency_key) across every op state, so two
    // successive edits of ONE event must carry distinct keys to both run — the reason the
    // key is a caller-supplied argument, not derived from the event.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let base = stored("/cal/default/evt-6.ics", "evt-6@test.local");

    let mut ops = Vec::new();
    for key in ["patch:evt-6:rev1", "patch:evt-6:rev2"] {
        ops.push(
            patch_calendar_event(
                &provider,
                &store,
                &account(),
                worker(),
                Duration::from_mins(1),
                key,
                &base,
                PatchTarget::Series,
                EventPatch::new(stamp()).summary("Renamed"),
            )
            .await
            .unwrap(),
        );
    }

    // Two distinct durable ops, both terminal-success — the second edit was not collapsed
    // into the first.
    assert_ne!(ops[0].op, ops[1].op);
    assert_eq!(
        store.pending_op_state(ops[1].op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[test]
fn the_durable_payload_records_the_intent_not_the_rendered_bytes() {
    // This is what makes a conflict recoverable: the op holds *which occurrence, and what
    // changed*, so a retry re-applies it to a freshly fetched base. Had it stored the
    // document the edit produced, the retry would re-send bytes built on the very copy the
    // server has moved past — reverting somebody else's edit with a write it accepts.
    let base = stored("/cal/default/evt-7.ics", "evt-7@test.local");
    let edit = EventEdit::new(
        &base,
        PatchTarget::Series,
        EventPatch::new(stamp()).summary("Renamed").end(at(11)),
    );
    let payload = serde_json::to_value(&edit).unwrap();
    assert_eq!(serde_json::from_value::<EventEdit>(payload).unwrap(), edit);

    let draft = draft("evt-7@test.local");
    let payload = serde_json::to_value(&draft).unwrap();
    assert_eq!(
        serde_json::from_value::<EventDraft>(payload).unwrap(),
        draft
    );

    let deletion = EventDeletion::of(&base);
    let payload = serde_json::to_value(&deletion).unwrap();
    assert_eq!(
        serde_json::from_value::<EventDeletion>(payload).unwrap(),
        deletion
    );
}
