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
use serde_json::Value;

/// A JMAP protocol or transport failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JmapError {
    /// The HTTP exchange itself failed (connect, TLS, a body, or a server that went quiet
    /// for longer than the shared deadlines allow).
    #[error("JMAP transport error: {0}")]
    Transport(#[from] engine_http::SendError),

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
    #[error(
        "JMAP set error '{error_type}' for object '{object_id}'{}",
        named_properties(properties)
    )]
    Set {
        /// The id (or creation id) of the object that failed.
        object_id: String,
        /// The JMAP `SetError` `type` string.
        error_type: String,
        /// The properties the server named as at fault (`invalidProperties` carries them),
        /// empty when it named none. Property names only: a `SetError`'s `description` is
        /// the server's own prose and may quote the values it refused.
        properties: Vec<String>,
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

    /// A scope larger than one request moved under every attempt to page it whole, so no
    /// attempt is a complete answer to store. Carries the type name; the next pass tries again.
    #[error("{0}/query kept changing while it was paged")]
    Unsettled(String),

    /// A `/set` that may have been applied: its request may have reached the server and no
    /// answer of the server's own about the object came back. Becomes
    /// [`ProviderError::needs_confirmation`].
    #[error("{0}; the change may have been applied")]
    Unconfirmed(Box<JmapError>),
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
            properties: Vec::new(),
        }
    }

    /// Builds a [`JmapError::Set`] from the `SetError` object a server answered for
    /// `object_id` (RFC 8620 §5.3), keeping its `type` and the properties it names.
    pub(crate) fn refused(object_id: impl Into<String>, set_error: &Value) -> Self {
        Self::Set {
            object_id: object_id.into(),
            error_type: set_error_type(set_error).to_owned(),
            properties: set_error
                .get("properties")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        }
    }

    /// What this error is as the failure of a submission: the class it always has, except
    /// for an exchange that failed after the request may have reached the server. That one
    /// may have sent the message, so it needs confirming and is never retried
    /// (`ProviderError::needs_confirmation`).
    ///
    /// Two more are the same case. A gateway's `502` or `504` says the server behind it gave
    /// no answer, not that it refused; every other status is the server's own answer. And a
    /// `200` whose body cannot be read is an answer nobody can read.
    pub(crate) fn into_submission_error(self) -> ProviderError {
        match &self {
            Self::Transport(err) if err.may_have_been_received() => {
                let detail = format!("{self}; the message may have been sent");
                ProviderError::needs_confirmation(detail).with_source(self)
            }
            Self::Status {
                status: 502 | 504, ..
            }
            | Self::Json(_)
            | Self::Protocol(_)
            | Self::MissingResponse(_) => {
                let detail = format!("{self}; the message may have been sent");
                ProviderError::needs_confirmation(detail).with_source(self)
            }
            _ => self.into(),
        }
    }

    /// What this error is as the failure to read a submission's answer, after the server
    /// may have acted on it: a refusal the answer states (a `SetError`, a method error) keeps
    /// its class, and an answer that cannot be read leaves the send ambiguous.
    pub(crate) fn into_answer_error(self) -> ProviderError {
        match &self {
            Self::Set { .. } | Self::Method { .. } => self.into(),
            _ => {
                let detail = format!("{self}; the message may have been sent");
                ProviderError::needs_confirmation(detail).with_source(self)
            }
        }
    }

    /// What this error means as the end of a `/set` that changes a calendar or a contact:
    /// [`Unconfirmed`](Self::Unconfirmed) where the change may have been applied, and itself
    /// where it was not.
    ///
    /// It may have been when the request may have reached the server and the exchange then
    /// failed; when a gateway answered `502` or `504`, which says the server behind it gave
    /// no answer; when the answer cannot be read, or carries no response to the call; and on
    /// `serverPartialFail`, which RFC 8620 §3.6.2 defines as some of the changes having been
    /// made. A refusal the answer states (any other method error or `SetError`, any other
    /// status) is the server's own, and the change was not made.
    pub(crate) fn after_write(self) -> Self {
        let unknown = match &self {
            Self::Transport(err) => err.may_have_been_received(),
            Self::Status { status, .. } => matches!(status, 502 | 504),
            Self::Json(_) | Self::Protocol(_) | Self::MissingResponse(_) => true,
            Self::Method { error_type, .. } | Self::Set { error_type, .. } => {
                error_type == "serverPartialFail"
            }
            Self::Session(_) | Self::Unsettled(_) | Self::Unconfirmed(_) => false,
        };
        if unknown {
            Self::Unconfirmed(Box::new(self))
        } else {
            self
        }
    }

    /// A `/set` answer that names `target` in neither of the maps that would say what became
    /// of it (RFC 8620 §5.3 requires one of them), so the change may or may not have been made.
    pub(crate) fn unanswered(target: &str, done: &str, refused: &str) -> Self {
        Self::Unconfirmed(Box::new(Self::protocol(format!(
            "the answer named '{target}' in neither {done} nor {refused}"
        ))))
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, body, .. } => status_class(*status, body),
            // A malformed response/session is a protocol-level incompatibility:
            // retrying the same request will not fix it.
            Self::Json(_) | Self::Protocol(_) | Self::Session(_) | Self::MissingResponse(_) => {
                FailureClass::Permanent
            }
            Self::Method { error_type, .. } => method_class(error_type),
            Self::Set { error_type, .. } => set_error_class(error_type),
            Self::Unsettled(_) => FailureClass::Retryable,
            // Never retried as it stands: the first attempt may have landed.
            Self::Unconfirmed(_) => FailureClass::InvalidState,
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

/// Maps a failed exchange to a [`FailureClass`]: a body that did not decode is a protocol
/// problem; every other failure, a server gone quiet included, is transient.
fn transport_class(err: &engine_http::SendError) -> FailureClass {
    if err.is_decode() {
        FailureClass::Permanent
    } else {
        FailureClass::Retryable
    }
}

/// A client error outside any exchange, such as a client that could not be built.
impl From<reqwest::Error> for JmapError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.into())
    }
}

impl From<JmapError> for ProviderError {
    fn from(err: JmapError) -> Self {
        if matches!(err, JmapError::Unconfirmed(_)) {
            return ProviderError::needs_confirmation(err.to_string()).with_source(err);
        }
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

/// The `SetError` a `/set` result reports for `target` under `map` (`notCreated`,
/// `notUpdated` or `notDestroyed`), if any (RFC 8620 §5.3).
pub(crate) fn set_refusal<'a>(result: &'a Value, map: &str, target: &str) -> Option<&'a Value> {
    result.get(map).and_then(|refused| refused.get(target))
}

/// A `SetError`'s `type`, or `unknown` when it carries none.
pub(crate) fn set_error_type(set_error: &Value) -> &str {
    set_error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

/// The ` (properties: a, b)` suffix a [`JmapError::Set`] message carries when it names any.
fn named_properties(properties: &[String]) -> String {
    if properties.is_empty() {
        String::new()
    } else {
        format!(" (properties: {})", properties.join(", "))
    }
}
