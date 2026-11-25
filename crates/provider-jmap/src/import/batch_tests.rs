//! The import as a whole, offline: how a batch is packed into requests the server takes, how
//! uploads run, and what a failed request or upload answers.

use std::sync::atomic::Ordering;

use engine_core::error::FailureClass;
use serde_json::{Value, json};

use super::{scripted::*, *};

#[tokio::test]
async fn a_failed_upload_answers_its_message_alone_and_never_as_possibly_stored() {
    // A server error is about that upload; a throttle or a refused credential would end the
    // batch (`a_throttled_or_refused_upload_ends_the_batch_before_any_import`).
    let (provider, scripted) = target(
        session(|_| {}),
        vec![Ok(stored(&[1], &[1]))],
        vec![Err(JmapError::status(500, "{\"detail\":\"secret text\"}"))],
    );
    let batch = [message(8_000_000, 1), message(10, 1)];
    let answers = provider.import_messages(&account(), &batch).await;

    let failed = answers[0].as_ref().unwrap_err();
    assert_eq!(failed.class(), FailureClass::Retryable);
    assert!(!failed.requires_confirmation());
    assert!(
        !failed.detail().contains("secret"),
        "no server text: {failed}"
    );
    assert_eq!(key(&answers[1]), "M1");
    let sent = scripted.sent.lock().unwrap();
    assert!(sent[0]["methodCalls"][1][1]["emails"].get("m0").is_none());
}

/// Uploads overlap, as many at once as the session's `maxConcurrentUpload`, and no more.
#[tokio::test]
async fn uploads_run_as_wide_as_the_session_allows() {
    let two_at_once = |doc: &mut Value| {
        doc["capabilities"]["urn:ietf:params:jmap:core"]["maxConcurrentUpload"] = json!(2);
    };
    let (provider, scripted) = target(
        session(two_at_once),
        vec![],
        (0..5).map(|n| Ok(format!("U{n}"))).collect(),
    );
    let batch: Vec<_> = (0..5).map(|_| message(7_499_489, 1)).collect();
    let answers = provider.import_messages(&account(), &batch).await;
    assert!(answers.iter().all(Result::is_ok));
    assert_eq!(scripted.peak.load(Ordering::SeqCst), 2);
    // Each answered under its own message, whatever order the uploads finished in.
    let sent = scripted.sent.lock().unwrap();
    for n in 0..5 {
        assert_eq!(
            sent[0]["methodCalls"][0][1]["emails"][format!("m{n}")]["blobId"],
            format!("U{n}")
        );
    }
}

/// An upload whose answer was lost stored a blob at most, never a message: a message is stored
/// only by the `Email/import` that references it, which was never sent.
#[tokio::test]
async fn an_upload_whose_answer_was_lost_did_not_store_its_message() {
    let (provider, scripted) = target(
        session(|_| {}),
        vec![],
        vec![Err(JmapError::protocol("an unreadable answer"))],
    );
    let answers = provider
        .import_messages(&account(), &[message(8_000_000, 1)])
        .await;
    let lost = answers[0].as_ref().unwrap_err();
    assert!(!lost.requires_confirmation(), "{lost}");
    assert_eq!(lost.class(), FailureClass::Permanent);
    assert!(scripted.sent.lock().unwrap().is_empty());
}

/// The packing is checked against the bytes that actually go out: under a 4,000-byte
/// `maxSizeRequest`, for a sweep of message sizes, every request fits, and none could have
/// taken the next message too, so the budget is neither overrun nor wasted.
#[tokio::test]
async fn every_request_fits_the_request_size_the_session_states_and_no_more_would() {
    const LIMIT: usize = 4_000;
    let small = |doc: &mut Value| {
        doc["capabilities"]["urn:ietf:params:jmap:core"]["maxSizeRequest"] = json!(LIMIT);
    };
    for len in (300..1_400).step_by(37) {
        let batch: Vec<_> = (0..7).map(|n| message(len, 1 + n % 3)).collect();
        let (provider, scripted) = target(session(small), vec![], vec![]);
        let answers = provider.import_messages(&account(), &batch).await;
        for (n, answer) in answers.iter().enumerate() {
            assert_eq!(key(answer), format!("M{n}"), "{len}");
        }
        let sent = scripted.sent.lock().unwrap();
        let sizes: Vec<usize> = sent
            .iter()
            .map(|request| serde_json::to_vec(request).unwrap().len())
            .collect();
        assert!(sizes.iter().all(|&size| size <= LIMIT), "{len}: {sizes:?}");
        // The least a message adds: its base64 and a minimal entry. The budget is conservative
        // (the envelope and each entry are reserved at their longest), so a request may stop
        // short of the limit by that slack, and no more.
        let least = engine_rfc5322::base64_encode(&vec![b'x'; len]).len() + 60;
        let slack = 400;
        assert!(
            sizes[..sizes.len() - 1]
                .iter()
                .all(|&size| size + least + slack > LIMIT),
            "{len}: requests left room for another message: {sizes:?}"
        );
    }
}

/// A request that may have reached the server answers its messages as possibly stored, and
/// ends the batch: the messages after it are answered as not sent.
#[tokio::test]
async fn a_failed_request_ends_the_batch() {
    let one_per_request = |doc: &mut Value| {
        doc["capabilities"]["urn:ietf:params:jmap:core"]["maxObjectsInSet"] = json!(1);
    };
    let (provider, scripted) = target(
        session(one_per_request),
        vec![
            Ok(stored(&[0], &[0])),
            Err(JmapError::protocol("an unreadable answer")),
        ],
        vec![],
    );
    let batch: Vec<_> = (0..3).map(|_| message(10, 1)).collect();
    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(key(&answers[0]), "M0");
    assert!(answers[1].as_ref().unwrap_err().requires_confirmation());
    let unsent = answers[2].as_ref().unwrap_err();
    assert_eq!(unsent.class(), FailureClass::Retryable);
    assert!(!unsent.requires_confirmation());
    assert_eq!(scripted.sent.lock().unwrap().len(), 2);
}

/// A throttled request was refused, so nothing it carried is stored, and the wait it names
/// reaches every message behind it.
#[tokio::test]
async fn a_throttled_request_carries_its_wait_to_the_messages_behind_it() {
    let one_per_request = |doc: &mut Value| {
        doc["capabilities"]["urn:ietf:params:jmap:core"]["maxObjectsInSet"] = json!(1);
    };
    let wait = Some(std::time::Duration::from_secs(43));
    let (provider, _) = target(
        session(one_per_request),
        vec![Err(JmapError::status(429, "{}").with_retry_after(wait))],
        vec![],
    );
    let batch: Vec<_> = (0..2).map(|_| message(10, 1)).collect();
    let answers = provider.import_messages(&account(), &batch).await;
    for answer in &answers {
        let err = answer.as_ref().unwrap_err();
        assert_eq!(err.class(), FailureClass::RateLimited);
        assert_eq!(err.retry_after(), wait.map(Into::into));
        assert!(!err.requires_confirmation());
    }
    assert!(
        answers[0]
            .as_ref()
            .unwrap_err()
            .detail()
            .contains("HTTP 429")
    );
}

/// An exchange that failed before the request left: nothing reached the server, so the
/// messages were not stored and may be sent again.
#[tokio::test]
async fn a_request_that_never_left_is_retryable() {
    let (provider, _) = target(
        session(|_| {}),
        vec![Err(JmapError::Transport(engine_http::SendError::withheld(
            "the hand-over was refused".to_owned(),
        )))],
        vec![],
    );
    let answers = provider
        .import_messages(&account(), &[message(10, 1)])
        .await;
    let err = answers[0].as_ref().unwrap_err();
    assert!(!err.requires_confirmation(), "{err}");
    assert!(err.detail().contains("the exchange failed"), "{err}");
}

fn one_upload_at_a_time(doc: &mut Value) {
    doc["capabilities"]["urn:ietf:params:jmap:core"]["maxConcurrentUpload"] = json!(1);
}

/// A throttle or a refused credential at the upload URL reaches every message of the account:
/// the uploads stop, no import is sent, and every message not yet answered carries it.
#[tokio::test]
async fn a_throttled_or_refused_upload_ends_the_batch_before_any_import() {
    let wait = Some(std::time::Duration::from_secs(1206));
    for (failure, class) in [
        (
            JmapError::status(429, "{}").with_retry_after(wait),
            FailureClass::RateLimited,
        ),
        (JmapError::status(401, "{}"), FailureClass::Authentication),
    ] {
        let (provider, scripted) = target(
            session(one_upload_at_a_time),
            vec![],
            vec![Err(failure), Ok("U1".to_owned()), Ok("U2".to_owned())],
        );
        let batch = [
            message(8_000_000, 1),
            message(8_000_000, 1),
            message(10, 1),
            message(8_000_000, 1),
        ];
        let answers = provider.import_messages(&account(), &batch).await;

        assert_eq!(scripted.uploaded.lock().unwrap().len(), 1, "{class:?}");
        assert!(scripted.sent.lock().unwrap().is_empty(), "{class:?}");
        for answer in &answers {
            let err = answer.as_ref().unwrap_err();
            assert_eq!(err.class(), class);
            assert!(!err.requires_confirmation());
        }
        for unsent in &answers[1..] {
            assert!(
                unsent
                    .as_ref()
                    .unwrap_err()
                    .detail()
                    .starts_with("not sent")
            );
        }
        if class == FailureClass::RateLimited {
            assert!(
                answers
                    .iter()
                    .all(|a| a.as_ref().unwrap_err().retry_after() == wait.map(Into::into))
            );
        }
    }
}

/// The upload quota refusing an inline blob ends the batch as a `429` from the upload URL
/// would: the next request is not sent, and its messages carry the throttle.
#[tokio::test]
async fn an_inline_blob_over_the_upload_quota_sends_nothing_after_it() {
    let one_per_request = |doc: &mut Value| {
        doc["capabilities"]["urn:ietf:params:jmap:core"]["maxObjectsInSet"] = json!(1);
    };
    let (provider, scripted) = target(
        session(one_per_request),
        vec![Ok(json!({ "methodResponses": [
            ["Blob/upload", { "notCreated": { "b0": { "type": "overQuota" } } }, "0"],
            ["error", { "type": "invalidResultReference" }, "1"]
        ] }))],
        vec![],
    );
    let batch = [message(10, 1), message(10, 1)];
    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(scripted.sent.lock().unwrap().len(), 1);
    for answer in &answers {
        assert_eq!(
            answer.as_ref().unwrap_err().class(),
            FailureClass::RateLimited
        );
    }
    assert!(
        answers[1]
            .as_ref()
            .unwrap_err()
            .detail()
            .starts_with("not sent")
    );
}

/// No credential could be had for an upload: the batch ends as on a refused credential, and every
/// message says that no credential could be had, not that the target refused one.
#[tokio::test]
async fn an_upload_without_a_credential_ends_the_batch_and_says_so() {
    let no_credential =
        crate::credential_test_support::no_credential(FailureClass::Authentication).await;
    let (provider, scripted) = target(
        session(one_upload_at_a_time),
        vec![],
        vec![Err(no_credential), Ok("U1".to_owned())],
    );
    let batch = [message(8_000_000, 1), message(8_000_000, 1)];
    let answers = provider.import_messages(&account(), &batch).await;

    assert!(scripted.sent.lock().unwrap().is_empty());
    for answer in &answers {
        let err = answer.as_ref().unwrap_err();
        assert_eq!(err.class(), FailureClass::Authentication);
        assert!(err.is_credential_failure(), "{err:?}");
        assert!(!err.detail().contains("target"), "{err}");
    }
}
