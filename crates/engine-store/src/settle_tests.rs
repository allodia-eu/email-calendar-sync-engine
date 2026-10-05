use super::*;

fn now() -> UtcDateTime {
    "2026-01-01T00:00:00Z".parse().unwrap()
}

fn retryable() -> PendingOutcome {
    PendingOutcome::Failed {
        class: FailureClass::Retryable,
        retry_after: None,
    }
}

#[test]
fn a_send_never_settles_on_its_attempt_count() {
    let send = Some(PendingOpKind::MailSubmit);
    let parked = record_outcome(send, MAX_ATTEMPTS * 10, &retryable(), now()).unwrap();
    assert_eq!(parked.state, PendingOpState::Pending);
    assert!(parked.next_attempt_at.is_some());

    // Every other write still stops after its bound.
    let edit = Some(PendingOpKind::MailEdit);
    let settled = record_outcome(edit, MAX_ATTEMPTS - 1, &retryable(), now()).unwrap();
    assert_eq!(settled.state, PendingOpState::Failed);
}

#[test]
fn a_class_no_retry_fixes_settles_a_send_at_once() {
    let refused = PendingOutcome::Failed {
        class: FailureClass::Authentication,
        retry_after: None,
    };
    let recorded = record_outcome(Some(PendingOpKind::MailSubmit), 0, &refused, now()).unwrap();
    assert_eq!(recorded.state, PendingOpState::Failed);
    assert_eq!(recorded.failure_class, Some(FailureClass::Authentication));
    assert!(!recorded.keeps_hand_over);
}

#[test]
fn only_an_ambiguous_outcome_keeps_the_hand_over() {
    let ambiguous = PendingOutcome::NeedsConfirmation {
        detail: "lost".to_owned(),
    };
    let kept = record_outcome(Some(PendingOpKind::MailSubmit), 0, &ambiguous, now()).unwrap();
    assert!(kept.keeps_hand_over);
    assert_eq!(kept.detail.as_deref(), Some("lost"));

    for definitive in [
        retryable(),
        PendingOutcome::Succeeded {
            provider_key: engine_core::ids::ProviderKey::new("k").unwrap(),
        },
    ] {
        let recorded =
            record_outcome(Some(PendingOpKind::MailSubmit), 0, &definitive, now()).unwrap();
        assert!(!recorded.keeps_hand_over, "{definitive:?}");
    }
}

#[test]
fn an_interrupted_send_turns_on_whether_it_had_handed_over() {
    let send = PendingOpKind::MailSubmit;
    assert!(matches!(
        interrupted_outcome(send, true),
        PendingOutcome::NeedsConfirmation { .. }
    ));
    // Not handed over: an ordinary retry, due at once.
    let retry = interrupted_outcome(send, false);
    let recorded = record_outcome(Some(send), 0, &retry, now()).unwrap();
    assert_eq!(recorded.state, PendingOpState::Pending);
    assert_eq!(recorded.next_attempt_at, Some(now()));

    // Any other write backs off, hand-over or not: it never records one.
    for handed_over in [false, true] {
        let edit = interrupted_outcome(PendingOpKind::MailEdit, handed_over);
        let recorded = record_outcome(Some(PendingOpKind::MailEdit), 0, &edit, now()).unwrap();
        assert_eq!(recorded.state, PendingOpState::Pending);
        assert!(recorded.next_attempt_at.is_some_and(|due| due > now()));
    }
}

#[test]
fn a_failed_send_is_listed_and_every_other_settled_op_is_not() {
    let send = Some(PendingOpKind::MailSubmit);
    assert!(stays_listed(send, PendingOpState::Failed));
    assert!(stays_listed(send, PendingOpState::NeedsConfirmation));
    assert!(!stays_listed(send, PendingOpState::Succeeded));
    assert!(!stays_listed(send, PendingOpState::Cancelled));
    assert!(!stays_listed(
        Some(PendingOpKind::MailEdit),
        PendingOpState::Failed
    ));
    assert!(!stays_listed(None, PendingOpState::Failed));
    assert!(stays_listed(None, PendingOpState::Pending));
}
