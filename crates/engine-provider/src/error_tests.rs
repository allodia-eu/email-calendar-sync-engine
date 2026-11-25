//! Offline tests for [`ProviderError`]: its classes, and the facts it carries beside them.

use super::*;

#[test]
fn class_named_constructors_set_the_right_class() {
    assert_eq!(
        ProviderError::authentication("no token").class(),
        FailureClass::Authentication
    );
    assert_eq!(
        ProviderError::needs_resync("cannotCalculateChanges").class(),
        FailureClass::NeedsResync
    );
    assert!(ProviderError::retryable("503").is_retryable());
    assert!(!ProviderError::permanent("notFound").is_retryable());
    assert!(ProviderError::needs_resync("reset").requires_resync());
}

#[test]
fn rate_limited_carries_retry_after() {
    let after: Duration = "PT30S".parse().unwrap();
    let err = ProviderError::rate_limited("slow down", Some(after));
    assert_eq!(err.class(), FailureClass::RateLimited);
    assert_eq!(err.retry_after(), Some(after));
    assert!(err.is_retryable());
}

#[test]
fn every_constructor_and_accessor_is_exercised() {
    assert_eq!(
        ProviderError::retryable("a").class(),
        FailureClass::Retryable
    );
    assert_eq!(ProviderError::conflict("b").class(), FailureClass::Conflict);
    let permanent = ProviderError::permanent("c");
    assert_eq!(permanent.detail(), "c");
    assert!(!permanent.requires_resync() && !permanent.is_retryable());
    assert!(ProviderError::needs_resync("d").requires_resync());
    assert!(
        ProviderError::new(FailureClass::InvalidState, "e")
            .retry_after()
            .is_none()
    );
}

#[test]
fn needs_confirmation_is_ambiguous_and_not_retryable() {
    let err = ProviderError::needs_confirmation("post-DATA acknowledgement lost");
    assert!(err.requires_confirmation());
    // Ambiguity must never be plain-retried (it might have delivered).
    assert!(!err.is_retryable());
    assert_eq!(err.class(), FailureClass::InvalidState);
    // An ordinary failure is not flagged for confirmation.
    assert!(!ProviderError::permanent("550 rejected").requires_confirmation());
}

#[test]
fn an_unavailable_item_is_permanent_and_says_so_while_a_plain_permanent_does_not() {
    let err = ProviderError::item_unavailable("the server cannot give this message back");
    assert!(err.is_item_unavailable());
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(!ProviderError::permanent("403 Forbidden").is_item_unavailable());
}

#[test]
fn source_is_preserved_and_reachable() {
    let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out");
    let err = ProviderError::retryable("transport").with_source(io);
    // Display carries the class + detail; the source chain carries the cause.
    assert!(err.to_string().contains("transport"));
    assert!(std::error::Error::source(&err).is_some());
}

/// The target refused because of its own state, which waiting does not change: the class
/// is `InvalidState`, with no wait, and the reason is there to read back, under a detail
/// of the engine's own words.
#[test]
fn a_refusal_by_the_target_s_state_says_which_and_is_not_a_wait() {
    let reasons = [
        TargetRefusal::StorageFull,
        TargetRefusal::NoSuchContainer,
        TargetRefusal::ReadOnlyContainer,
    ];
    let mut details = std::collections::BTreeSet::new();
    for reason in reasons {
        let refused = ProviderError::refused_by_target(reason);
        assert_eq!(refused.class(), FailureClass::InvalidState);
        assert_eq!(refused.target_refusal(), Some(reason));
        assert_eq!(refused.retry_after(), None);
        assert!(!refused.is_retryable());
        assert!(!refused.is_item_unavailable() && !refused.requires_confirmation());
        details.insert(refused.detail().to_owned());
    }
    assert_eq!(details.len(), reasons.len(), "each reason says itself");
    assert_eq!(ProviderError::invalid_state("x").target_refusal(), None);
    assert_eq!(
        ProviderError::refused_by_target(TargetRefusal::StorageFull)
            .with_source(std::io::Error::other("x"))
            .target_refusal(),
        Some(TargetRefusal::StorageFull)
    );
}

/// Only a credential failure says so: no constructor a server's or a transport's failure is
/// built with sets it, whatever the class.
#[test]
fn no_failure_of_the_server_or_the_transport_is_a_credential_failure() {
    let after: Duration = "PT30S".parse().unwrap();
    for error in [
        ProviderError::retryable("connection reset"),
        ProviderError::authentication("401"),
        ProviderError::rate_limited("429", Some(after)),
        ProviderError::permanent("403"),
        ProviderError::needs_confirmation("lost"),
        ProviderError::item_unavailable("too large"),
        ProviderError::refused_by_target(TargetRefusal::StorageFull),
    ] {
        assert!(!error.is_credential_failure(), "{error}");
    }
    let marked = ProviderError::retryable("broker down").of_credential();
    assert!(marked.is_credential_failure());
    assert_eq!(marked.class(), FailureClass::Retryable);
}

/// Rewording changes the words and drops the source, and keeps every fact a caller reads.
#[test]
fn a_reworded_failure_keeps_every_fact_and_only_changes_its_words() {
    let after: Duration = "PT30S".parse().unwrap();
    let throttled = ProviderError::rate_limited("server text", Some(after))
        .of_credential()
        .with_source(std::io::Error::other("x"));
    let reworded = throttled.reworded("not stored: RateLimited");
    assert_eq!(reworded.detail(), "not stored: RateLimited");
    assert_eq!(reworded.class(), FailureClass::RateLimited);
    assert_eq!(reworded.retry_after(), Some(after));
    assert!(reworded.is_credential_failure());
    assert!(std::error::Error::source(&reworded).is_none());

    let lost = ProviderError::needs_confirmation("lost").reworded("x");
    assert!(lost.requires_confirmation() && !lost.is_credential_failure());
    assert!(
        ProviderError::item_unavailable("big")
            .reworded("x")
            .is_item_unavailable()
    );
    assert_eq!(
        ProviderError::refused_by_target(TargetRefusal::NoSuchContainer)
            .reworded("x")
            .target_refusal(),
        Some(TargetRefusal::NoSuchContainer)
    );
}

/// A batch ends on what reaches every item after it: a throttle, a refused credential, or no
/// credential at all, whatever the source's class. Nothing about one item or one exchange ends
/// it.
#[test]
fn a_batch_ends_on_a_throttle_a_refused_credential_or_no_credential() {
    for ending in [
        ProviderError::rate_limited("429", None),
        ProviderError::authentication("401"),
        ProviderError::retryable("the broker is down").of_credential(),
        ProviderError::permanent("not permitted").of_credential(),
    ] {
        assert!(ending.ends_batch(), "{ending:?}");
    }
    for one in [
        ProviderError::retryable("connection reset"),
        ProviderError::item_unavailable("too large"),
        ProviderError::needs_confirmation("lost"),
        ProviderError::permanent("403"),
    ] {
        assert!(!one.ends_batch(), "{one:?}");
    }
}
