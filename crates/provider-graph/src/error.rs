//! Microsoft Graph protocol errors and their classification into the engine
//! taxonomy.
//!
//! [`GraphError`] is the rich protocol-level error. At the provider-trait boundary
//! it converts into an [`engine_provider::ProviderError`] carrying an
//! engine-neutral [`FailureClass`], so callers branch on the class and never on
//! Graph specifics (`providers.md`). Graph error bodies are a documented
//! `{ "error": { "code", "message" } }` envelope; the `code` is captured for
//! diagnostics, and the HTTP status drives classification — including `410 Gone`
//! for an expired/invalid delta token, the analogue of JMAP
//! `cannotCalculateChanges`, which forces a full resync.
//!
//! The HTTP transport and its `reqwest`-error classification land with the
//! transport layer; this module covers the protocol/status surface that the pure
//! normalizers and (next) the fetch layer produce.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;
use serde_json::Value;

/// A Microsoft Graph protocol failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GraphError {
    /// The HTTP request itself failed (connect, timeout, TLS, or body decode).
    #[error("Graph transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server returned a non-success HTTP status. The Graph error `code` is
    /// captured when the body carried the standard envelope, and the raw body is
    /// kept for diagnostics.
    #[error("Graph HTTP {status} (code {code:?}): {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The Graph error `code`, when the body carried the standard envelope.
        code: Option<String>,
        /// The raw response body.
        body: String,
    },

    /// A response was not the JSON the protocol requires.
    #[error("malformed Graph JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// A response was structurally not what the protocol requires (a missing `id`,
    /// an absent `value` array or `@odata.deltaLink`, …).
    #[error("malformed Graph response: {0}")]
    Protocol(String),
}

impl GraphError {
    /// Builds a [`GraphError::Status`], extracting the Graph error `code` from the
    /// standard `{ "error": { "code", "message" } }` body when present.
    #[must_use]
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        let code = error_code(&body);
        Self::Status { status, code, body }
    }

    /// Builds a [`GraphError::Protocol`].
    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol(detail.into())
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, .. } => status_class(*status),
            // Malformed JSON or a structurally invalid response is a protocol-level
            // incompatibility: retrying the same request will not fix it.
            Self::Json(_) | Self::Protocol(_) => FailureClass::Permanent,
        }
    }
}

/// Maps a reqwest transport error to a [`FailureClass`]: a body that did not decode
/// is a permanent protocol mismatch; connect/timeout/request failures are transient.
fn transport_class(err: &reqwest::Error) -> FailureClass {
    if err.is_decode() {
        FailureClass::Permanent
    } else {
        FailureClass::Retryable
    }
}

/// Extracts the Graph error `code` from a `{ "error": { "code": .. } }` body, or
/// `None` when the body is not the standard envelope.
fn error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value.get("error")?.get("code")?.as_str().map(str::to_owned)
}

/// Maps an HTTP status to a [`FailureClass`]. Graph throttles with `429`
/// (+ `Retry-After`); an expired/invalid delta token is `410 Gone`, which forces a
/// full resync (the analogue of JMAP `cannotCalculateChanges`).
fn status_class(status: u16) -> FailureClass {
    match status {
        401 => FailureClass::Authentication,
        410 => FailureClass::NeedsResync,
        429 => FailureClass::RateLimited,
        500..=599 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    }
}

impl From<GraphError> for ProviderError {
    fn from(err: GraphError) -> Self {
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAD_REQUEST: &str = include_str!("../tests/fixtures/error/bad_request.json");
    const UNAUTHORIZED: &str = include_str!("../tests/fixtures/error/unauthorized.json");

    #[test]
    fn real_error_bodies_yield_their_code_and_class() {
        // The captured 400 and 401 envelopes carry their documented codes.
        let bad = GraphError::status(400, BAD_REQUEST);
        assert!(matches!(&bad, GraphError::Status { code: Some(c), .. } if c == "BadRequest"));
        assert_eq!(bad.failure_class(), FailureClass::Permanent);

        let unauth = GraphError::status(401, UNAUTHORIZED);
        assert!(
            matches!(&unauth, GraphError::Status { code: Some(c), .. } if c == "InvalidAuthenticationToken")
        );
        assert_eq!(unauth.failure_class(), FailureClass::Authentication);
        // Display carries the status, code, and body.
        assert!(unauth.to_string().contains("401"));
        assert!(unauth.to_string().contains("InvalidAuthenticationToken"));
    }

    #[test]
    fn status_codes_map_to_engine_classes() {
        // 410 Gone (expired delta token) forces a resync; 429 throttles; 5xx retries.
        assert_eq!(
            GraphError::status(410, "{}").failure_class(),
            FailureClass::NeedsResync
        );
        assert_eq!(
            GraphError::status(429, "{}").failure_class(),
            FailureClass::RateLimited
        );
        assert_eq!(
            GraphError::status(503, "{}").failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            GraphError::status(404, "{}").failure_class(),
            FailureClass::Permanent
        );
    }

    #[test]
    fn a_body_without_the_envelope_has_no_code() {
        // A non-JSON body and a JSON body without an `error.code` both yield None.
        assert!(matches!(
            GraphError::status(400, "not json at all"),
            GraphError::Status { code: None, .. }
        ));
        assert!(matches!(
            GraphError::status(500, r#"{"unexpected":true}"#),
            GraphError::Status { code: None, .. }
        ));
    }

    #[test]
    fn protocol_and_json_errors_are_permanent() {
        let protocol = GraphError::protocol("response had no value array");
        assert_eq!(protocol.failure_class(), FailureClass::Permanent);
        assert!(protocol.to_string().contains("no value array"));

        let json: GraphError = serde_json::from_str::<Value>("{ not json")
            .unwrap_err()
            .into();
        assert_eq!(json.failure_class(), FailureClass::Permanent);
        assert!(json.to_string().contains("malformed Graph JSON"));
    }

    #[test]
    fn converts_into_classified_provider_error_with_source() {
        let provider: ProviderError = GraphError::status(401, UNAUTHORIZED).into();
        assert_eq!(provider.class(), FailureClass::Authentication);
        // The original Graph error stays reachable through the source chain.
        assert!(std::error::Error::source(&provider).is_some());
    }
}
