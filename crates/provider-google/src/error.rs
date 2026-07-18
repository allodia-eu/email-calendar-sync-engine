//! Google (Gmail + Calendar) protocol errors and their classification into the
//! engine taxonomy.
//!
//! [`GoogleError`] is the rich protocol-level error. At the provider-trait boundary
//! it converts into an [`engine_provider::ProviderError`] carrying an engine-neutral
//! [`FailureClass`], so callers branch on the class and never on Google specifics
//! (`providers.md`). Google error bodies are a documented
//! `{ "error": { "code", "message", "status", "errors": [{ "reason" }] } }`
//! envelope; the machine `reason` (or the canonical `status`) is captured for
//! diagnostics and for the one place the HTTP status alone is ambiguous — a `403`,
//! which is a **rate limit** when its reason says so and an insufficient-permission
//! **permanent** failure otherwise.
//!
//! Two status codes drive a resync-style recovery, and they differ by API:
//! - **Google Calendar** returns `410 Gone` for an expired `syncToken`
//!   ([`FailureClass::NeedsResync`], the analogue of Graph's `410` / JMAP's
//!   `cannotCalculateChanges`).
//! - **Gmail** returns `404` for a `startHistoryId` that has aged out of the history window; that
//!   is *not* globally a resync (a `404` on a message `get` just means the message is gone), so the
//!   Gmail fetch layer special-cases a `404` from the `history.list` call rather than classifying
//!   every `404` here.

use engine_core::error::FailureClass;
use engine_provider::ProviderError;
use serde_json::Value;

/// A Google (Gmail / Calendar) protocol failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GoogleError {
    /// The HTTP request itself failed (connect, timeout, TLS, or body decode).
    #[error("Google transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server returned a non-success HTTP status. The machine `reason` (or the
    /// canonical `status`) is captured when the body carried the standard envelope,
    /// and the raw body is kept for diagnostics.
    #[error("Google HTTP {status} (reason {reason:?}): {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The Google error `reason` (or canonical `status`), when present.
        reason: Option<String>,
        /// The raw response body.
        body: String,
    },

    /// The Gmail `startHistoryId` has aged out of the history window (a `404` from
    /// `history.list`): a delta from that cursor is impossible, forcing a full resync.
    /// This is Gmail's analogue of Calendar's `410`/Graph's `410`/JMAP's
    /// `cannotCalculateChanges`, kept distinct from a plain `404` (a gone message) which
    /// stays [`Permanent`](FailureClass::Permanent).
    #[error("Gmail history expired (a full resync is required): {0}")]
    HistoryExpired(String),

    /// A response was not the JSON the protocol requires.
    #[error("malformed Google JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// A response was structurally not what the protocol requires (a missing `id`,
    /// an absent list array, a malformed value, …).
    #[error("malformed Google response: {0}")]
    Protocol(String),
}

impl GoogleError {
    /// Builds a [`GoogleError::Status`], extracting the machine `reason` (the first
    /// `error.errors[].reason`, falling back to the canonical `error.status`) from the
    /// standard body when present.
    #[must_use]
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        let body = body.into();
        let reason = error_reason(&body);
        Self::Status {
            status,
            reason,
            body,
        }
    }

    /// Builds a [`GoogleError::Protocol`].
    #[must_use]
    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol(detail.into())
    }

    /// Builds a [`GoogleError::HistoryExpired`] (an aged-out Gmail `startHistoryId`).
    #[must_use]
    pub fn history_expired(detail: impl Into<String>) -> Self {
        Self::HistoryExpired(detail.into())
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, reason, .. } => status_class(*status, reason.as_deref()),
            // An aged-out Gmail historyId forces a full resync.
            Self::HistoryExpired(_) => FailureClass::NeedsResync,
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

/// Extracts the machine `reason` from a Google error envelope: the first
/// `error.errors[].reason`, else the canonical `error.status`, else `None`.
fn error_reason(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let first_reason = error
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|e| e.get("reason"))
        .and_then(Value::as_str);
    first_reason
        .or_else(|| error.get("status").and_then(Value::as_str))
        .map(str::to_owned)
}

/// `true` when a `403`'s reason marks a rate/quota limit rather than a permission
/// failure — the one case the HTTP status alone cannot classify.
fn is_rate_limit_reason(reason: Option<&str>) -> bool {
    matches!(
        reason,
        Some(
            "rateLimitExceeded" | "userRateLimitExceeded" | "dailyLimitExceeded" | "quotaExceeded"
        )
    )
}

/// Maps an HTTP status (and, for `403`, its reason) to a [`FailureClass`]. Google
/// throttles with `429` and, on some APIs, `403 rateLimitExceeded`; Calendar returns
/// `410 Gone` for an expired `syncToken` (forcing a full resync); a failed write
/// precondition — a stale `If-Match` ETag on a calendar write — is `409`/`412`, a
/// [`Conflict`](FailureClass::Conflict) resolved by refetch-and-retry.
fn status_class(status: u16, reason: Option<&str>) -> FailureClass {
    match status {
        401 => FailureClass::Authentication,
        // A 403 is a rate limit only when its reason says so; otherwise it is an
        // insufficient-permission failure, which falls to the permanent default.
        403 if is_rate_limit_reason(reason) => FailureClass::RateLimited,
        409 | 412 => FailureClass::Conflict,
        410 => FailureClass::NeedsResync,
        429 => FailureClass::RateLimited,
        500..=599 => FailureClass::Retryable,
        _ => FailureClass::Permanent,
    }
}

impl From<GoogleError> for ProviderError {
    fn from(err: GoogleError) -> Self {
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNAUTHORIZED: &str = include_str!("../tests/fixtures/error/unauthorized.json");
    const RATE_LIMIT: &str = include_str!("../tests/fixtures/error/rate_limit.json");
    const FORBIDDEN: &str = include_str!("../tests/fixtures/error/forbidden.json");
    const HISTORY_GONE: &str = include_str!("../tests/fixtures/error/history_gone.json");
    const SYNC_TOKEN_GONE: &str = include_str!("../tests/fixtures/error/sync_token_gone.json");

    #[test]
    fn real_error_bodies_yield_their_reason_and_class() {
        let unauth = GoogleError::status(401, UNAUTHORIZED);
        assert!(matches!(&unauth, GoogleError::Status { reason: Some(r), .. } if r == "authError"));
        assert_eq!(unauth.failure_class(), FailureClass::Authentication);
        // Display carries the status, reason, and body.
        assert!(unauth.to_string().contains("401"));
        assert!(unauth.to_string().contains("authError"));
    }

    #[test]
    fn a_403_is_a_rate_limit_only_when_its_reason_says_so() {
        // A 403 whose reason is a rate/quota limit throttles (retry with backoff)…
        let limited = GoogleError::status(403, RATE_LIMIT);
        assert!(
            matches!(&limited, GoogleError::Status { reason: Some(r), .. } if r == "rateLimitExceeded")
        );
        assert_eq!(limited.failure_class(), FailureClass::RateLimited);
        // …while a 403 for insufficient scopes is permanent (never retried).
        let forbidden = GoogleError::status(403, FORBIDDEN);
        assert_eq!(forbidden.failure_class(), FailureClass::Permanent);
    }

    #[test]
    fn calendar_410_needs_resync_but_gmail_404_stays_permanent() {
        // Calendar's expired syncToken → 410 → a full resync.
        let sync_gone = GoogleError::status(410, SYNC_TOKEN_GONE);
        assert_eq!(sync_gone.failure_class(), FailureClass::NeedsResync);
        // Gmail's aged-out historyId → 404, which is *not* globally a resync (a 404 on
        // a message get just means gone); the fetch layer special-cases the history call
        // and converts it into the explicit HistoryExpired signal.
        let history_gone = GoogleError::status(404, HISTORY_GONE);
        assert_eq!(history_gone.failure_class(), FailureClass::Permanent);
        let expired = GoogleError::history_expired("startHistoryId too old");
        assert_eq!(expired.failure_class(), FailureClass::NeedsResync);
        assert!(expired.to_string().contains("full resync"));
    }

    #[test]
    fn status_codes_map_to_engine_classes() {
        assert_eq!(
            GoogleError::status(429, "{}").failure_class(),
            FailureClass::RateLimited
        );
        assert_eq!(
            GoogleError::status(503, "{}").failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            GoogleError::status(412, "{}").failure_class(),
            FailureClass::Conflict
        );
        assert_eq!(
            GoogleError::status(409, "{}").failure_class(),
            FailureClass::Conflict
        );
        assert_eq!(
            GoogleError::status(400, "{}").failure_class(),
            FailureClass::Permanent
        );
    }

    #[test]
    fn a_body_without_the_envelope_has_no_reason() {
        assert!(matches!(
            GoogleError::status(400, "not json at all"),
            GoogleError::Status { reason: None, .. }
        ));
        assert!(matches!(
            GoogleError::status(500, r#"{"unexpected":true}"#),
            GoogleError::Status { reason: None, .. }
        ));
    }

    #[test]
    fn protocol_and_json_errors_are_permanent() {
        let protocol = GoogleError::protocol("response had no messages array");
        assert_eq!(protocol.failure_class(), FailureClass::Permanent);
        assert!(protocol.to_string().contains("no messages array"));

        let json: GoogleError = serde_json::from_str::<Value>("{ not json")
            .unwrap_err()
            .into();
        assert_eq!(json.failure_class(), FailureClass::Permanent);
        assert!(json.to_string().contains("malformed Google JSON"));
    }

    #[test]
    fn converts_into_classified_provider_error_with_source() {
        let provider: ProviderError = GoogleError::status(401, UNAUTHORIZED).into();
        assert_eq!(provider.class(), FailureClass::Authentication);
        assert!(std::error::Error::source(&provider).is_some());
    }
}
