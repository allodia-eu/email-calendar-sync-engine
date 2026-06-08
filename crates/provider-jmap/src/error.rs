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

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, .. } => status_class(*status),
            // A malformed response/session is a protocol-level incompatibility:
            // retrying the same request will not fix it.
            Self::Json(_) | Self::Protocol(_) | Self::Session(_) | Self::MissingResponse(_) => {
                FailureClass::Permanent
            }
            Self::Method { error_type, .. } => method_class(error_type),
        }
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

/// Maps an HTTP status to a [`FailureClass`].
fn status_class(status: u16) -> FailureClass {
    match status {
        401 => FailureClass::Authentication,
        429 => FailureClass::RateLimited,
        500..=599 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
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
