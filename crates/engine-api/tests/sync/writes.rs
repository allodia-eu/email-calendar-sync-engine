//! Outbox-mediated writes through the facade: mail submission, edits and reports
//! recorded as durable ops (success committing `Succeeded`, failure surfacing as a sync
//! error), and the pending-op state poll for an unknown op.

use engine_api::{ApiError, Engine, PendingOpId, PendingOpState};

use super::*;

#[tokio::test]
async fn submit_mail_records_a_successful_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let draft = draft("gen-1@test.local", "Quarterly report");

    let outcome = engine
        .submit_mail(&provider, &account(), &draft)
        .await
        .unwrap();
    assert_eq!(outcome.email_key, ProviderKey::new("sent-1").unwrap());
    assert_eq!(outcome.message_id, draft.message_id);
    assert!(outcome.sent_copy.is_filed());
    // The durable op committed Succeeded, pollable by the returned id.
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// A message that was delivered but whose sender's copy could not be filed is a **success
/// with a caveat**, and the facade has to carry both halves: the op commits `Succeeded` (the
/// mail has gone — an op the outbox could retry would re-send it) while the outcome says the
/// copy is missing. Collapsing either half is how a Sent copy got lost in silence.
#[tokio::test]
async fn a_delivered_send_reports_an_unfiled_copy_without_failing_the_op() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: true,
    };
    let draft = draft("gen-2@test.local", "Quarterly report");

    let outcome = engine
        .submit_mail(&provider, &account(), &draft)
        .await
        .unwrap();

    assert!(!outcome.sent_copy.is_filed());
    assert_eq!(
        outcome.sent_copy.unfiled_detail(),
        Some("APPEND failed: connection reset")
    );
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded),
        "the send completed; only the copy is missing"
    );
}

#[tokio::test]
async fn submit_mail_surfaces_a_failed_send() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
        unfiled: false,
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
        unfiled: false,
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
        unfiled: false,
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
async fn report_message_records_a_successful_report() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let target = ProviderKey::new("imap:v1:u42@INBOX").unwrap();

    let outcome = engine
        .report_message(
            &provider,
            &account(),
            "report:u42:junk",
            &MessageReport::new(
                target.clone(),
                ReportVerdict::Junk,
                MailboxId::try_from("Junk").unwrap(),
            ),
        )
        .await
        .unwrap();

    // A report names the source key, like a move: where the filing mints a new key the
    // destination's next sync reconciles the copy.
    assert_eq!(outcome.message_key, target);
    assert_eq!(
        engine.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// The two directions are **separate ops**, not one op re-run. A key derived from the
/// target alone would collapse a junk report and the user's later correction into one
/// enqueue, and the correction would silently never reach the provider.
#[tokio::test]
async fn a_correction_is_its_own_op_not_a_replay_of_the_report() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let target = ProviderKey::new("imap:v1:u42@INBOX").unwrap();
    let junk = MailboxId::try_from("Junk").unwrap();
    let inbox = MailboxId::try_from("INBOX").unwrap();

    let reported = engine
        .report_message(
            &provider,
            &account(),
            "report:u42:junk",
            &MessageReport::new(target.clone(), ReportVerdict::Junk, junk),
        )
        .await
        .unwrap();
    let corrected = engine
        .report_message(
            &provider,
            &account(),
            "report:u42:notjunk",
            &MessageReport::new(target, ReportVerdict::NotJunk, inbox),
        )
        .await
        .unwrap();

    assert_ne!(reported.op, corrected.op, "two intents, two durable ops");
    assert_eq!(
        engine.pending_op_state(corrected.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// Replaying the *same* intent does not report twice. The enqueue is idempotent on the
/// caller's key, so the second attempt re-finds the first op — which has already
/// committed `Succeeded` and is therefore no longer claimable, and the attempt refuses
/// instead of reaching the provider a second time.
///
/// Asserted here rather than assumed: this is the whole difference between an outbox
/// retry and a duplicate report, and the refusal is what a caller sees, so it must be
/// the documented shape rather than an incidental error.
#[tokio::test]
async fn replaying_one_report_intent_does_not_report_again() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let report = MessageReport::new(
        ProviderKey::new("imap:v1:u42@INBOX").unwrap(),
        ReportVerdict::Junk,
        MailboxId::try_from("Junk").unwrap(),
    );

    let first = engine
        .report_message(&provider, &account(), "report:u42:junk", &report)
        .await
        .unwrap();
    let err = engine
        .report_message(&provider, &account(), "report:u42:junk", &report)
        .await
        .unwrap_err();

    // The error names the *first* op, which is the evidence of deduplication: a second
    // enqueue would have minted a new id.
    assert!(
        err.to_string().contains(&format!("{:?}", first.op)),
        "the replay re-found op {:?}, got {err}",
        first.op
    );
    assert_eq!(
        engine.pending_op_state(first.op).await.unwrap(),
        Some(PendingOpState::Succeeded),
        "and left the completed op alone"
    );
}

#[tokio::test]
async fn report_message_surfaces_a_failed_report() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
        unfiled: false,
    };
    // A failed report surfaces as a sync error; the outbox records the op `Failed`
    // before returning (locked at engine-sync), exactly as for an edit.
    let err = engine
        .report_message(
            &provider,
            &account(),
            "report:u42:junk",
            &MessageReport::new(
                ProviderKey::new("imap:v1:u42@INBOX").unwrap(),
                ReportVerdict::Junk,
                MailboxId::try_from("Junk").unwrap(),
            ),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");
}

/// A verdict the provider's controls exclude is refused, and the refusal is recorded as
/// a failed op rather than swallowed. Gmail is the live case: it has no phishing label,
/// so a host that ignored `Capabilities::mail_report` would otherwise see a report that
/// looked accepted and reached nothing.
#[tokio::test]
async fn a_verdict_the_provider_does_not_offer_fails_the_op() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let err = engine
        .report_message(
            &provider,
            &account(),
            "report:u42:phishing",
            &MessageReport::new(
                ProviderKey::new("imap:v1:u42@INBOX").unwrap(),
                ReportVerdict::Phishing,
                MailboxId::try_from("Junk").unwrap(),
            ),
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
