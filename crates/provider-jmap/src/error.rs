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
        /// When the server said to come back, from [`engine_http::Sent::stated_wait`].
        ///
        /// Usually `None`, and honestly so: RFC 8620 §3.6.1 names *which* limit was hit
        /// and says nothing about when it clears, so the concurrency refusal this adapter
        /// classifies carries no instant. A server may still send a `Retry-After` on an
        /// ordinary `429` — Stalwart does on a full blob store, asking for 2688 seconds —
        /// and that is a number the outbox should obey rather than average down.
        retry_after: Option<core::time::Duration>,
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

    /// The adapter refused a request before sending it, because the protocol says the
    /// client must not: a draft whose `From` no identity on the account may send as.
    #[error("refused before sending: {0}")]
    Refused(String),
}

impl JmapError {
    /// Builds a [`JmapError::Status`].
    pub(crate) fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Status {
            status,
            body: body.into(),
            retry_after: None,
        }
    }

    /// Records when the server said this would clear, from [`engine_http::Sent::stated_wait`].
    #[must_use]
    pub(crate) fn with_retry_after(mut self, wait: Option<core::time::Duration>) -> Self {
        if let Self::Status { retry_after, .. } = &mut self {
            *retry_after = wait;
        }
        self
    }

    /// When the server said to come back, if it said.
    #[must_use]
    pub(crate) fn retry_after(&self) -> Option<core::time::Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
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
            Self::Status { status, body, .. } => status_class(*status, body),
            // A malformed response/session is a protocol-level incompatibility:
            // retrying the same request will not fix it.
            Self::Json(_)
            | Self::Protocol(_)
            | Self::Session(_)
            | Self::MissingResponse(_)
            | Self::Refused(_) => FailureClass::Permanent,
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
        // A rate limit is the one class that carries *when*: the outbox obeys a provider's
        // instant outright, past its own derived cap, and a host schedules from it.
        let error = if class == FailureClass::RateLimited {
            ProviderError::rate_limited(detail, err.retry_after().map(Into::into))
        } else {
            ProviderError::new(class, detail)
        };
        error.with_source(err)
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
