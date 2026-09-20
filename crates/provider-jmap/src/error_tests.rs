//! How a JMAP protocol failure classifies, and what it carries with it.
//!
//! Its own file for the 500-line limit: `error.rs` states the taxonomy, this states
//! what the taxonomy has to answer for each shape a real server sends.

use super::*;
#[test]
fn method_errors_classify_per_rfc_8620() {
    assert_eq!(
        JmapError::Method {
            call_id: "1".into(),
            error_type: "cannotCalculateChanges".into(),
        }
        .failure_class(),
        FailureClass::NeedsResync
    );
    assert_eq!(
        JmapError::Method {
            call_id: "0".into(),
            error_type: "rateLimit".into(),
        }
        .failure_class(),
        FailureClass::RateLimited
    );
    assert_eq!(
        JmapError::Method {
            call_id: "0".into(),
            error_type: "stateMismatch".into(),
        }
        .failure_class(),
        FailureClass::Conflict
    );
    assert_eq!(
        JmapError::Method {
            call_id: "0".into(),
            error_type: "unknownMethod".into(),
        }
        .failure_class(),
        FailureClass::Permanent
    );
}

#[test]
fn a_concurrency_limit_is_a_throttle_even_though_it_arrives_as_a_400() {
    // Observed against Stalwart when a body warm overlapped more downloads than the
    // session advertised. Read as its bare status this is Permanent, and a body
    // dropped as permanent is one no later pass fetches again.
    let refused = JmapError::Status {
        status: 400,
        body: r#"{"type":"urn:ietf:params:jmap:error:limit","status":400,
                 "detail":"The request exceeds the maximum number of concurrent requests.",
                 "limit":"maxConcurrentRequests"}"#
            .to_owned(),
        retry_after: None,
    };
    assert_eq!(refused.failure_class(), FailureClass::RateLimited);
}

/// The three `urn:ietf:params:jmap:error:limit` refusals Stalwart was made to send, and
/// the `429` it answers when the blob store itself is full. Captured 2026-09-20 by
/// tripping each limit on the harness in turn: concurrent uploads for the first, 17
/// calls in one request for the second, an 11 MB request for the third.
///
/// The first is the interesting one, and it took some finding. Stalwart enforces
/// `maxConcurrentRequests` on **uploads** — 300 simultaneous API calls and 300
/// simultaneous blob *downloads* were all answered `200` — and reports it under the
/// `maxConcurrentRequests` name even though the session advertises a separate
/// `maxConcurrentUpload`. One counter, two names.
const CONCURRENCY: &str = include_str!("../tests/fixtures/error/concurrency_limit.json");
const TOO_MANY_CALLS: &str = include_str!("../tests/fixtures/error/calls_in_request_limit.json");
const TOO_LARGE: &str = include_str!("../tests/fixtures/error/size_request_limit.json");
const BLOB_QUOTA: &str = include_str!("../tests/fixtures/error/blob_quota.json");

#[test]
fn the_send_funnel_waits_out_the_400_the_adapter_already_called_a_rate_limit() {
    use engine_http::ThrottleClassifier as _;

    // The two verdicts have to agree, because they are the same verdict: one names the
    // failure for the caller, the other decides whether to wait. #218 is what it cost
    // when only the first existed.
    assert_eq!(
        JmapError::Status {
            status: 400,
            body: CONCURRENCY.to_owned(),
            retry_after: None,
        }
        .failure_class(),
        FailureClass::RateLimited,
    );
    assert_eq!(
        JmapThrottles.throttle(400, CONCURRENCY.as_bytes()),
        Some(engine_http::Throttle::new()),
    );
}

#[test]
fn the_request_shaped_limits_are_not_waited_out_either() {
    use engine_http::ThrottleClassifier as _;

    // The same type, the same status, and the opposite answer: these describe the
    // request that was sent, so re-sending it unchanged fails identically and a wait
    // buys nothing but the wait. Pinned against the server's real bytes in both
    // directions, because one direction alone cannot show the reader is discriminating.
    for body in [TOO_MANY_CALLS, TOO_LARGE] {
        assert_eq!(JmapThrottles.throttle(400, body.as_bytes()), None, "{body}");
    }
}

#[test]
fn the_blob_quota_429_carries_the_instant_stalwart_named() {
    use engine_provider::ProviderError;

    // Stalwart asks for 2688 seconds — three quarters of an hour — when the blob store
    // is full. That is past the outbox's own 30-minute cap, so without the instant it
    // would retry fifteen minutes early and be refused again. `retry_delay` obeys a
    // provider's number outright for exactly this case; this is the first time any
    // adapter has given it one.
    let refused = JmapError::Status {
        status: 429,
        body: BLOB_QUOTA.to_owned(),
        retry_after: Some(core::time::Duration::from_secs(2688)),
    };
    let neutral: ProviderError = refused.into();
    assert_eq!(neutral.class(), FailureClass::RateLimited);
    assert_eq!(
        neutral
            .retry_after()
            .map(engine_core::time::Duration::seconds),
        Some(2688),
    );
}

#[test]
fn the_concurrency_400_names_no_instant_and_does_not_pretend_to() {
    use engine_provider::ProviderError;

    // RFC 8620 §3.6.1 says *which* limit was hit and nothing about when it clears, so
    // the honest answer is `None` and a derived backoff. Inventing one here would put
    // a guess in front of a host as the server's word.
    let refused = JmapError::Status {
        status: 400,
        body: CONCURRENCY.to_owned(),
        retry_after: None,
    };
    let neutral: ProviderError = refused.into();
    assert_eq!(neutral.class(), FailureClass::RateLimited);
    assert_eq!(neutral.retry_after(), None);
}

#[test]
fn the_blob_quota_429_needs_no_classifier_at_all() {
    use engine_http::ThrottleClassifier as _;

    // Stalwart's other refusal, met by the same probe: a full blob store, answered
    // `429` with a `Retry-After` of about three quarters of an hour. Nothing here has
    // to recognise it — the status rule waits out a `429` whatever sent it, and the
    // policy's budget then declines a wait that long and hands the work to the next
    // pass, which for a quota measured in hours is the only sane answer.
    assert!(!JmapThrottles.reads_body_of(429));
    assert_eq!(
        JmapError::Status {
            status: 429,
            body: BLOB_QUOTA.to_owned(),
            retry_after: None,
        }
        .failure_class(),
        FailureClass::RateLimited,
    );
}

#[test]
fn only_the_400_is_worth_reading_a_body_for() {
    use engine_http::ThrottleClassifier as _;

    assert!(JmapThrottles.reads_body_of(400));
    for settled in [200_u16, 401, 404, 429, 500, 503] {
        assert!(!JmapThrottles.reads_body_of(settled), "{settled}");
    }
    // And nothing that is not the problem-details shape is read as one.
    for body in [&b""[..], b"<html>bad request</html>", b"{}", b"\xff\xfe"] {
        assert_eq!(JmapThrottles.throttle(400, body), None, "{body:?}");
    }
}

#[test]
fn the_other_request_limits_stay_permanent() {
    // The same error type, and the opposite answer: these two describe the request
    // that was sent, so re-sending it unchanged fails identically.
    for limit in ["maxSizeRequest", "maxCallsInRequest"] {
        let refused = JmapError::Status {
            status: 400,
            body: format!(r#"{{"type":"urn:ietf:params:jmap:error:limit","limit":"{limit}"}}"#),
            retry_after: None,
        };
        assert_eq!(refused.failure_class(), FailureClass::Permanent, "{limit}");
    }
    // And a plain 400 with no problem details at all.
    assert_eq!(
        JmapError::Status {
            status: 400,
            body: "not json".to_owned(),
            retry_after: None,
        }
        .failure_class(),
        FailureClass::Permanent
    );
}

#[test]
fn set_errors_classify_per_rfc_8620() {
    // A target that moved on server-side is a conflict → re-sync then retry.
    assert_eq!(
        JmapError::set("e1", "notFound").failure_class(),
        FailureClass::Conflict
    );
    assert_eq!(
        JmapError::set("e1", "stateMismatch").failure_class(),
        FailureClass::Conflict
    );
    // Quota/rate pushback is retryable after backoff.
    assert_eq!(
        JmapError::set("e1", "overQuota").failure_class(),
        FailureClass::RateLimited
    );
    assert_eq!(
        JmapError::set("e1", "serverPartialFail").failure_class(),
        FailureClass::Retryable
    );
    // A request the server keeps rejecting is permanent.
    assert_eq!(
        JmapError::set("e1", "forbidden").failure_class(),
        FailureClass::Permanent
    );
    assert_eq!(
        JmapError::set("e1", "invalidPatch").failure_class(),
        FailureClass::Permanent
    );
}

#[test]
fn http_status_maps_to_class() {
    assert_eq!(
        JmapError::status(401, "no auth").failure_class(),
        FailureClass::Authentication
    );
    assert_eq!(
        JmapError::status(429, "slow").failure_class(),
        FailureClass::RateLimited
    );
    assert_eq!(
        JmapError::status(503, "down").failure_class(),
        FailureClass::Retryable
    );
    assert_eq!(
        JmapError::status(400, "bad").failure_class(),
        FailureClass::Permanent
    );
}

#[test]
fn converts_into_classified_provider_error_with_source() {
    let provider: ProviderError = JmapError::Method {
        call_id: "2".into(),
        error_type: "cannotCalculateChanges".into(),
    }
    .into();
    assert_eq!(provider.class(), FailureClass::NeedsResync);
    assert!(provider.requires_resync());
    assert!(std::error::Error::source(&provider).is_some());
}

#[test]
fn malformed_responses_are_permanent() {
    assert_eq!(
        JmapError::protocol("methodResponses missing").failure_class(),
        FailureClass::Permanent
    );
    assert_eq!(
        JmapError::session("no apiUrl").failure_class(),
        FailureClass::Permanent
    );
    assert_eq!(
        JmapError::MissingResponse("9".into()).failure_class(),
        FailureClass::Permanent
    );
}
