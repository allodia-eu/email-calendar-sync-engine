//! JMAP protocol errors and their classification into the engine taxonomy.
//!
//! [`JmapError`] is the rich protocol-level error (transport, HTTP status, JSON,
//! method error, malformed session). At the provider-trait boundary it converts
//! into an [`engine_provider::ProviderError`] carrying an engine-neutral
//! [`FailureClass`], so callers branch on the class and never on JMAP specifics
//! (`providers.md`). The mapping follows RFC 8620 §3.6.2 (request/method errors)
//! and the provider classification in `providers.md`.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

/// A JMAP protocol or transport failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JmapError {
    /// The HTTP request itself failed (connect, timeout, TLS, body).
    #[error("JMAP transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server returned a non-success HTTP status. The body is captured for
    /// diagnostics (a JMAP "problem details" document for request-level errors,
    /// RFC 8620 §3.6.1).
    #[error("JMAP HTTP {status}: {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body (possibly a JSON problem-details document).
        body: String,
    },

    /// A response (session or API) was not the JSON the protocol requires.
    #[error("malformed JMAP JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// A method returned an error response (`["error", { "type": .. }, callId]`,
    /// RFC 8620 §3.6.2) instead of its result.
    #[error("JMAP method error '{error_type}' for call '{call_id}'")]
    Method {
        /// The call id of the failed invocation.
        call_id: String,
        /// The JMAP error `type` string.
        error_type: String,
    },

    /// A `/set` method rejected an individual object with a `SetError` (RFC 8620
    /// §5.3) — the method call itself succeeded, but the create/update/destroy of one
    /// object failed. Classified per the set-error `type`.
    #[error("JMAP set error '{error_type}' for object '{object_id}'")]
    Set {
        /// The id (or creation id) of the object that failed.
        object_id: String,
        /// The JMAP `SetError` `type` string.
        error_type: String,
    },

    /// The batched response carried no entry for a call id that was sent.
    #[error("no method response for call '{0}'")]
    MissingResponse(String),

    /// A response was structurally not a valid JMAP envelope (e.g. `methodResponses`
    /// absent or not an array of triples).
    #[error("malformed JMAP response: {0}")]
    Protocol(String),

    /// The session resource was missing a required field.
    #[error("invalid JMAP session: {0}")]
    Session(String),
}

impl JmapError {
    /// Builds a [`JmapError::Status`].
    pub(crate) fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Status {
            status,
            body: body.into(),
        }
    }

    /// Builds a [`JmapError::Protocol`].
    pub(crate) fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol(detail.into())
    }

    /// Builds a [`JmapError::Session`].
    pub(crate) fn session(detail: impl Into<String>) -> Self {
        Self::Session(detail.into())
    }

    /// Builds a [`JmapError::Set`] from a rejected object id and its `SetError` type.
    pub(crate) fn set(object_id: impl Into<String>, error_type: impl Into<String>) -> Self {
        Self::Set {
            object_id: object_id.into(),
            error_type: error_type.into(),
        }
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, body } => status_class(*status, body),
            // A malformed response/session is a protocol-level incompatibility:
            // retrying the same request will not fix it.
            Self::Json(_) | Self::Protocol(_) | Self::Session(_) | Self::MissingResponse(_) => {
                FailureClass::Permanent
            }
            Self::Method { error_type, .. } => method_class(error_type),
            Self::Set { error_type, .. } => set_error_class(error_type),
        }
    }
}

/// Maps a JMAP `SetError` `type` (RFC 8620 §5.3) to a [`FailureClass`]. A `notFound`
/// or `stateMismatch` means the target moved on server-side — a
/// [`Conflict`](FailureClass::Conflict) the caller resolves by re-syncing then
/// retrying, exactly like an IMAP write against a stale UID. `rateLimit`/`overQuota`
/// are retryable-after-backoff; a server fault is retryable; everything else
/// (`forbidden`, `invalidProperties`, `invalidPatch`, `tooLarge`, …) is a request the
/// server will keep rejecting.
fn set_error_class(error_type: &str) -> FailureClass {
    match error_type {
        "notFound" | "stateMismatch" => FailureClass::Conflict,
        "rateLimit" | "overQuota" => FailureClass::RateLimited,
        "serverUnavailable" | "serverFail" | "serverPartialFail" => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    }
}

/// Maps a JMAP method-error `type` (RFC 8620 §3.6.2) to a [`FailureClass`].
fn method_class(error_type: &str) -> FailureClass {
    match error_type {
        // The cursor can no longer produce a delta — the scope must be resynced.
        "cannotCalculateChanges" => FailureClass::NeedsResync,
        "rateLimit" | "overQuota" => FailureClass::RateLimited,
        "serverUnavailable" | "serverFail" | "serverPartialFail" => FailureClass::Retryable,
        "stateMismatch" => FailureClass::Conflict,
        // accountNotFound / unknownMethod / invalidArguments / invalidResultReference
        // / unknownCapability / forbidden / accountNotSupportedByMethod and the rest
        // are request-shape or authorization problems that will not succeed unchanged.
        _ => FailureClass::Permanent,
    }
}

/// Maps an HTTP status to a [`FailureClass`], reading the request-level problem-details
/// body where the status alone is not the whole answer.
fn status_class(status: u16, body: &str) -> FailureClass {
    match status {
        401 => FailureClass::Authentication,
        400 if is_concurrency_limit(body) => FailureClass::RateLimited,
        429 => FailureClass::RateLimited,
        500..=599 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    }
}

/// Whether a `400` is the server saying "too many at once" rather than "this request is
/// wrong".
///
/// RFC 8620 §3.6.1 gives request-level errors as problem details, and returns *all* of them
/// with a `400` — including `urn:ietf:params:jmap:error:limit`, whose `limit` property names
/// which limit was hit. `maxConcurrentRequests` is the only one of the three a client can
/// clear by waiting: `maxSizeRequest` and `maxCallsInRequest` describe the request that was
/// sent, and re-sending it unchanged fails identically.
///
/// This matters because the status alone reads as permanent, and a body dropped as permanent
/// is a body no pass fetches again. Observed against Stalwart, which applies its
/// `maxConcurrentRequests` to blob downloads as well as to the API endpoint.
fn is_concurrency_limit(body: &str) -> bool {
    let Ok(problem) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    problem.get("type").and_then(serde_json::Value::as_str)
        == Some("urn:ietf:params:jmap:error:limit")
        && problem.get("limit").and_then(serde_json::Value::as_str) == Some("maxConcurrentRequests")
}

/// Reads JMAP's concurrency refusal out of the `400` that carries it.
///
/// The counterpart of [`status_class`]'s `400` arm, wired into the shared send funnel so the
/// refusal is *waited out* rather than merely named correctly. Before this the adapter
/// classified it `RateLimited` and handed it back, and the body went unfetched until the
/// next pass — #218.
///
/// Narrower than Google's classifier, because JMAP's problem details carry no reset instant:
/// RFC 8620 §3.6.1 defines `type` and, for `:limit`, which `limit` was hit, and nothing about
/// when it clears. A server that wants to name one has `Retry-After`, which the send funnel
/// already reads off the headers.
///
/// This is a belt to [`RequestGate`](engine_http::RequestGate)'s braces rather than a
/// replacement for it. `JmapClient::connect` narrows the account's gate to the session's
/// `maxConcurrentRequests`, so this adapter's own requests should not reach the limit at
/// all. They can anyway: a gate bounds one account *in this process*, and the server counts
/// the user's other clients too.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JmapThrottles;

impl engine_http::ThrottleClassifier for JmapThrottles {
    fn reads_body_of(&self, status: u16) -> bool {
        // The only status JMAP overloads. A `429` needs no body, and every other `400` here
        // — `maxSizeRequest`, `maxCallsInRequest`, a malformed envelope — describes the
        // request that was sent and fails identically however long anything waits.
        status == 400
    }

    fn throttle(&self, _status: u16, body: &[u8]) -> Option<engine_http::Throttle> {
        let body = core::str::from_utf8(body).ok()?;
        is_concurrency_limit(body).then(engine_http::Throttle::new)
    }
}

/// Maps a reqwest transport error to a [`FailureClass`]. Connect/timeout failures
/// are transient; a decode failure is a protocol problem.
fn transport_class(err: &reqwest::Error) -> FailureClass {
    if err.is_timeout() || err.is_connect() || err.is_request() {
        FailureClass::Retryable
    } else if err.is_decode() {
        FailureClass::Permanent
    } else {
        FailureClass::Retryable
    }
}

impl From<JmapError> for ProviderError {
    fn from(err: JmapError) -> Self {
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
}

#[cfg(test)]
mod tests {
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
    const TOO_MANY_CALLS: &str =
        include_str!("../tests/fixtures/error/calls_in_request_limit.json");
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
            };
            assert_eq!(refused.failure_class(), FailureClass::Permanent, "{limit}");
        }
        // And a plain 400 with no problem details at all.
        assert_eq!(
            JmapError::Status {
                status: 400,
                body: "not json".to_owned(),
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
}
