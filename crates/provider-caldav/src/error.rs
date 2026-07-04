//! CalDAV/WebDAV protocol errors and their classification into the engine taxonomy.
//!
//! [`CalDavError`] is the rich protocol-level error (transport, HTTP status,
//! malformed WebDAV XML, malformed iCalendar, structural protocol violations). At
//! the provider-trait boundary it converts into an
//! [`engine_provider::ProviderError`] carrying an engine-neutral [`FailureClass`],
//! so callers branch on the class and never on CalDAV specifics (`providers.md`).
//! The status mapping follows the WebDAV/CalDAV RFCs (4918 §11, 4791, 6578 §3.2).

use engine_core::error::FailureClass;
use engine_provider::ProviderError;

/// A CalDAV protocol or transport failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CalDavError {
    /// The HTTP request itself failed (connect, timeout, TLS, body).
    #[error("CalDAV transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server returned a non-success HTTP status. The body is captured for
    /// diagnostics and for detecting the RFC 6578 `valid-sync-token` precondition.
    #[error("CalDAV HTTP {status}: {body}")]
    Status {
        /// The HTTP status code.
        status: u16,
        /// The response body (possibly a WebDAV `DAV:error` document).
        body: String,
    },

    /// A WebDAV response body was not the XML the protocol requires.
    #[error("malformed WebDAV XML: {0}")]
    Xml(String),

    /// An iCalendar object resource (RFC 5545) could not be parsed.
    #[error("malformed iCalendar: {0}")]
    Ical(String),

    /// A response was structurally not a valid WebDAV multistatus (e.g. a
    /// `response` with no `href`, or a `PROPFIND` that returned no collections).
    #[error("malformed CalDAV response: {0}")]
    Protocol(String),
}

impl CalDavError {
    /// Builds a [`CalDavError::Status`].
    pub(crate) fn status(status: u16, body: impl Into<String>) -> Self {
        Self::Status {
            status,
            body: body.into(),
        }
    }

    /// Builds a [`CalDavError::Xml`].
    pub(crate) fn xml(detail: impl Into<String>) -> Self {
        Self::Xml(detail.into())
    }

    /// Builds a [`CalDavError::Ical`].
    pub(crate) fn ical(detail: impl Into<String>) -> Self {
        Self::Ical(detail.into())
    }

    /// Builds a [`CalDavError::Protocol`].
    pub(crate) fn protocol(detail: impl Into<String>) -> Self {
        Self::Protocol(detail.into())
    }

    /// The engine-neutral class this protocol error maps to.
    #[must_use]
    pub fn failure_class(&self) -> FailureClass {
        match self {
            Self::Transport(e) => transport_class(e),
            Self::Status { status, body } => status_class(*status, body),
            // A malformed response/object is a protocol-level incompatibility:
            // retrying the same request will not fix it.
            Self::Xml(_) | Self::Ical(_) | Self::Protocol(_) => FailureClass::Permanent,
        }
    }
}

/// Maps an HTTP status (with the body, for the sync-token precondition) to a
/// [`FailureClass`].
fn status_class(status: u16, body: &str) -> FailureClass {
    match status {
        // RFC 6578 §3.2: an invalid `sync-token` yields `403`/`409` carrying the
        // `DAV:valid-sync-token` precondition *element* — the scope must resync from
        // a snapshot rather than retry the delta unchanged. Matched as an element
        // (not a substring), so a genuine 403 merely mentioning the phrase is not
        // misclassified.
        403 | 409 if crate::dav::has_precondition(body, "valid-sync-token") => {
            FailureClass::NeedsResync
        }
        401 => FailureClass::Authentication,
        // `412 Precondition Failed` is an `If-Match`/ETag write conflict (RFC 4791
        // §5.3.2); a `409 Conflict` without the sync-token precondition is a
        // write-ordering conflict.
        409 | 412 => FailureClass::Conflict,
        423 | 500..=599 => FailureClass::Retryable, // Locked (RFC 4918 §11.3), or server error.
        429 => FailureClass::RateLimited,
        // A plain `403`, `404`, and anything else will not succeed unchanged.
        _ => FailureClass::Permanent,
    }
}

/// Maps a reqwest transport error to a [`FailureClass`]. Connect/timeout failures
/// are transient; a decode failure is a protocol problem.
fn transport_class(err: &reqwest::Error) -> FailureClass {
    if err.is_decode() {
        FailureClass::Permanent
    } else {
        FailureClass::Retryable
    }
}

impl From<CalDavError> for ProviderError {
    fn from(err: CalDavError) -> Self {
        let class = err.failure_class();
        let detail = err.to_string();
        ProviderError::new(class, detail).with_source(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_maps_to_class() {
        assert_eq!(
            CalDavError::status(401, "denied").failure_class(),
            FailureClass::Authentication
        );
        assert_eq!(
            CalDavError::status(429, "slow").failure_class(),
            FailureClass::RateLimited
        );
        assert_eq!(
            CalDavError::status(503, "down").failure_class(),
            FailureClass::Retryable
        );
        assert_eq!(
            CalDavError::status(412, "etag").failure_class(),
            FailureClass::Conflict
        );
        assert_eq!(
            CalDavError::status(404, "gone").failure_class(),
            FailureClass::Permanent
        );
    }

    #[test]
    fn invalid_sync_token_precondition_triggers_resync() {
        // RFC 6578 §3.2: the server rejects a stale token with this precondition,
        // and the engine must recover to a full snapshot.
        let body =
            "<?xml version=\"1.0\"?><D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>";
        assert_eq!(
            CalDavError::status(403, body).failure_class(),
            FailureClass::NeedsResync
        );
        assert_eq!(
            CalDavError::status(409, body).failure_class(),
            FailureClass::NeedsResync
        );
        // A plain 403 with no such precondition is a permanent authorization error.
        assert_eq!(
            CalDavError::status(403, "Forbidden").failure_class(),
            FailureClass::Permanent
        );
        // A genuine 403 whose body merely *mentions* the phrase in prose (not as a
        // precondition element) must NOT be misclassified as a resync.
        assert_eq!(
            CalDavError::status(403, "You lack a valid-sync-token privilege here").failure_class(),
            FailureClass::Permanent
        );
    }

    #[test]
    fn malformed_payloads_are_permanent() {
        assert_eq!(
            CalDavError::xml("unexpected eof").failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            CalDavError::ical("no VEVENT").failure_class(),
            FailureClass::Permanent
        );
        assert_eq!(
            CalDavError::protocol("response without href").failure_class(),
            FailureClass::Permanent
        );
    }

    #[test]
    fn converts_into_classified_provider_error_with_source() {
        let body = "<D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>";
        let provider: ProviderError = CalDavError::status(403, body).into();
        assert_eq!(provider.class(), FailureClass::NeedsResync);
        assert!(provider.requires_resync());
        assert!(std::error::Error::source(&provider).is_some());
    }
}
