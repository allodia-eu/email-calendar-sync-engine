//! Mail-mutation outbox-driver tests: enqueue-before-side-effect, success
//! recording, conflict-without-blind-retry, distinct-key serialization, and durable
//! payload round-trip. Uses the shared fakes/helpers from the parent via `use super::*`.

use super::*;

/// The message an edit targets (an IMAP-shaped key).
fn target() -> ProviderKey {
    ProviderKey::new("imap:v1:u42@INBOX").unwrap()
}

#[tokio::test]
async fn edit_mail_enqueues_then_applies_and_records_success() {
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let outcome = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:on",
        &MailEdit::mark_seen(target(), true),
    )
    .await
    .unwrap();

    // The edit resolved to the target message key and reached terminal success.
    assert_eq!(outcome.message_key, target());
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn edit_mail_records_conflict_without_blind_retry() {
    // A stale target (UIDVALIDITY changed) is recorded Failed (class Conflict) and
    // returned — the caller re-syncs and recomputes; the outbox does not blind-retry.
    let provider = FakeMail::new(vec![], vec![]).conflicting_writes();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:delete",
        &MailEdit::delete(target()),
    )
    .await
    .unwrap_err();
    match err {
        crate::SyncError::Provider(e) => {
            assert_eq!(e.class(), engine_core::error::FailureClass::Conflict);
        }
        other => panic!("expected a provider error, got {other:?}"),
    }

    // Recover the op id via an idempotent re-enqueue; it was recorded Failed. The
    // resource key is `mail:{target}`, serializing edits to one message.
    let op_id = store
        .enqueue_pending_op(
            account(),
            PendingOp::new(
                IdempotencyKey::new("edit:u42:delete").unwrap(),
                ResourceKey::new("mail:imap:v1:u42@INBOX").unwrap(),
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
async fn distinct_idempotency_keys_let_two_edits_of_one_message_both_run() {
    // The store dedups enqueue by (account, idempotency_key) across every op state,
    // so mark-read then mark-unread of ONE message must carry distinct keys to both
    // run — the reason the key is a caller-supplied argument.
    let provider = FakeMail::new(vec![], vec![]);
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let first = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:on",
        &MailEdit::mark_seen(target(), true),
    )
    .await
    .unwrap();
    let second = edit_mail(
        &provider,
        &store,
        &account(),
        worker(),
        Duration::from_mins(1),
        "edit:u42:seen:off",
        &MailEdit::mark_seen(target(), false),
    )
    .await
    .unwrap();

    assert_ne!(first.op, second.op);
    assert_eq!(
        store.pending_op_state(second.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[test]
fn edit_round_trips_through_a_durable_payload() {
    // The outbox stores the edit as a JSON payload; every variant must survive intact
    // for a recovery worker to re-apply it.
    let dest = MailboxId::try_from("Trash").unwrap();
    for original in [
        MailEdit::mark_seen(target(), true),
        MailEdit::set_flagged(target(), false),
        MailEdit::move_to(target(), dest),
        MailEdit::delete(target()),
    ] {
        let payload = serde_json::to_value(&original).unwrap();
        let restored: MailEdit = serde_json::from_value(payload).unwrap();
        assert_eq!(restored, original);
    }
}
