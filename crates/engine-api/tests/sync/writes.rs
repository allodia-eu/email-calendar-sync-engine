//! Outbox-mediated writes through the facade: mail submission and edits recorded as
//! durable ops (success committing `Succeeded`, failure surfacing as a sync error),
//! and the pending-op state poll for an unknown op.

use engine_api::{ApiError, Engine, PendingOpId, PendingOpState};

use super::*;

#[tokio::test]
async fn submit_mail_records_a_successful_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
    };
    let draft = draft("gen-1@test.local", "Quarterly report");

    let outcome = engine
        .submit_mail(&provider, &account(), &draft)
        .await
        .unwrap();
    assert_eq!(outcome.email_key, ProviderKey::new("sent-1").unwrap());
    assert_eq!(outcome.message_id, draft.message_id);
    // The durable op committed Succeeded, pollable by the returned id.
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn submit_mail_surfaces_a_failed_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
    };
    // A failed send surfaces as a sync error; the outbox records the op `Failed`
    // before returning (that recording is locked at the engine-sync layer).
    let err = engine
        .submit_mail(&provider, &account(), &draft("gen-2@test.local", "Lunch"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn edit_mail_records_a_successful_edit() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
    };
    let target = ProviderKey::new("imap:v1:u42@INBOX").unwrap();

    let outcome = engine
        .edit_mail(
            &provider,
            &account(),
            "edit:u42:seen:on",
            &MailEdit::mark_seen(target.clone(), true),
        )
        .await
        .unwrap();
    assert_eq!(outcome.message_key, target);
    // The durable op committed Succeeded, pollable by the returned id.
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn edit_mail_surfaces_a_failed_edit() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
    };
    // A failed edit (here a stale-target Conflict) surfaces as a sync error; the
    // outbox records the op `Failed` before returning (locked at engine-sync).
    let err = engine
        .edit_mail(
            &provider,
            &account(),
            "edit:u42:delete",
            &MailEdit::delete(ProviderKey::new("imap:v1:u42@INBOX").unwrap()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

#[tokio::test]
async fn pending_op_state_is_none_for_an_unknown_op() {
    let engine = Engine::open_in_memory().unwrap();
    assert_eq!(
        engine
            .pending_op_state(PendingOpId::new(999))
            .await
            .unwrap(),
        None
    );
}
