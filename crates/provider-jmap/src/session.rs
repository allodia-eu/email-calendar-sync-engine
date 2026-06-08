//! The JMAP session resource (RFC 8620 §2): capabilities, accounts, API URL, and
//! server limits.
//!
//! Two real-world subtleties this handles:
//!
//! - **The account id is looked up, not assumed.** The JMAP account id (e.g.
//!   `"c"`) is whatever the server assigned and is read from `primaryAccounts`
//!   per capability; it is distinct from the engine's host-assigned
//!   [`AccountId`](engine_core::ids::AccountId).
//! - **The advertised `apiUrl` may point at a different origin** than the one the
//!   client connected to (Stalwart advertises its configured public host,
//!   `https://mail.test.local/`, while tests connect to `127.0.0.1:18080`). The
//!   [`SessionUrlPolicy`] decides whether to trust the advertised origin or rebase
//!   it onto the connection base — the safe default for proxied / self-hosted /
//!   test setups.

use reqwest::Url;
use serde_json::Value;

use crate::error::JmapError;
use crate::request::capability;

/// How to resolve the session's advertised URLs against the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUrlPolicy {
    /// Replace the advertised origin (scheme/host/port) with the connection base,
    /// keeping only the path. Correct for reverse-proxied, self-hosted, and test
    /// servers that advertise a public hostname they are not reached at.
    RebaseToConnection,
    /// Use the advertised URL verbatim (RFC-literal). Correct when a provider
    /// genuinely serves its API from a different origin than the session.
    TrustAdvertised,
}

/// Server limits the client must respect when batching (RFC 8620 §1.5 core
/// capability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreLimits {
    /// Max objects fetchable in a single `/get` (`maxObjectsInGet`).
    pub max_objects_in_get: usize,
    /// Max objects settable in a single `/set` (`maxObjectsInSet`).
    pub max_objects_in_set: usize,
    /// Max method calls in one request (`maxCallsInRequest`).
    pub max_calls_in_request: usize,
}

impl Default for CoreLimits {
    fn default() -> Self {
        // Conservative RFC-floor-ish fallbacks if the server omits the core
        // capability (it never should). Keeps batching correct, just smaller.
        Self {
            max_objects_in_get: 100,
            max_objects_in_set: 100,
            max_calls_in_request: 16,
        }
    }
}

/// A parsed, connection-resolved JMAP session.
#[derive(Debug, Clone)]
pub struct Session {
    api_url: String,
    mail_account_id: Option<String>,
    submission_account_id: Option<String>,
    calendar_account_id: Option<String>,
    limits: CoreLimits,
    capabilities: engine_provider::Capabilities,
    state: Option<String>,
}

impl Session {
    /// Parses the session document, resolving its URLs against `base` per `policy`.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if `apiUrl` is absent or unparseable.
    pub(crate) fn parse(
        value: &Value,
        base: &Url,
        policy: SessionUrlPolicy,
    ) -> Result<Self, JmapError> {
        let advertised_api = value
            .get("apiUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| JmapError::session("apiUrl missing"))?;
        let api_url = resolve_against(base, advertised_api, policy)?;

        let primary = value.get("primaryAccounts");
        let account_for = |urn: &str| {
            primary
                .and_then(|p| p.get(urn))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };

        let caps = value.get("capabilities");
        let has = |urn: &str| caps.is_some_and(|c| c.get(urn).is_some());
        let capabilities = build_capabilities(has);

        let limits = caps
            .and_then(|c| c.get(capability::CORE))
            .map(parse_limits)
            .unwrap_or_default();

        Ok(Self {
            api_url,
            mail_account_id: account_for(capability::MAIL),
            submission_account_id: account_for(capability::SUBMISSION),
            calendar_account_id: account_for(capability::CALENDARS),
            limits,
            capabilities,
            state: value
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// The connection-resolved JMAP API endpoint to POST method calls to.
    #[must_use]
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    /// The JMAP account id for mail (the server's id, not the engine's).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if the server advertised no mail account.
    pub(crate) fn mail_account_id(&self) -> Result<&str, JmapError> {
        self.mail_account_id
            .as_deref()
            .ok_or_else(|| JmapError::session("no primary mail account"))
    }

    /// The JMAP account id for submission (`Identity`/`EmailSubmission`).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if the server advertised no submission account.
    pub(crate) fn submission_account_id(&self) -> Result<&str, JmapError> {
        self.submission_account_id
            .as_deref()
            .ok_or_else(|| JmapError::session("no primary submission account"))
    }

    /// The JMAP account id for calendars (`Calendar`/`CalendarEvent`).
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if the server advertised no calendar account.
    pub(crate) fn calendar_account_id(&self) -> Result<&str, JmapError> {
        self.calendar_account_id
            .as_deref()
            .ok_or_else(|| JmapError::session("no primary calendar account"))
    }

    /// The server's batching limits.
    #[must_use]
    pub fn limits(&self) -> CoreLimits {
        self.limits
    }

    /// The data domains the server advertises.
    #[must_use]
    pub fn capabilities(&self) -> engine_provider::Capabilities {
        self.capabilities
    }

    /// The opaque session state string (`state`), if present.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.state.as_deref()
    }
}

/// Resolves a `target` URL (absolute or a relative path) against the connection
/// `base` per the policy.
///
/// `base.join` already resolves a relative target against the base and lets an
/// absolute target win; [`SessionUrlPolicy::RebaseToConnection`] then forces the
/// origin back to the connection base, keeping only the path and query. Used for
/// both the session `apiUrl` and the well-known redirect `Location`.
pub(crate) fn resolve_against(
    base: &Url,
    target: &str,
    policy: SessionUrlPolicy,
) -> Result<String, JmapError> {
    let joined = base
        .join(target)
        .map_err(|e| JmapError::session(format!("bad URL {target:?}: {e}")))?;
    match policy {
        SessionUrlPolicy::TrustAdvertised => Ok(joined.into()),
        SessionUrlPolicy::RebaseToConnection => {
            let mut rebased = base.clone();
            rebased.set_path(joined.path());
            rebased.set_query(joined.query());
            Ok(rebased.into())
        }
    }
}

/// Builds the engine capability set from a "has this URN?" predicate.
fn build_capabilities(has: impl Fn(&str) -> bool) -> engine_provider::Capabilities {
    let mut caps = engine_provider::Capabilities::none();
    if has(capability::MAIL) {
        caps = caps.with_mail();
    }
    if has(capability::SUBMISSION) {
        caps = caps.with_submission();
    }
    if has(capability::CALENDARS) {
        caps = caps.with_calendars();
    }
    caps
}

/// Reads the core-capability limit fields, falling back to [`CoreLimits::default`]
/// per field.
fn parse_limits(core: &Value) -> CoreLimits {
    let defaults = CoreLimits::default();
    let read = |name: &str, fallback: usize| {
        core.get(name)
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|&v| v > 0)
            .unwrap_or(fallback)
    };
    CoreLimits {
        max_objects_in_get: read("maxObjectsInGet", defaults.max_objects_in_get),
        max_objects_in_set: read("maxObjectsInSet", defaults.max_objects_in_set),
        max_calls_in_request: read("maxCallsInRequest", defaults.max_calls_in_request),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A representative session subset, mirroring the live Stalwart shape (account
    /// id `"c"`, an advertised foreign `apiUrl`, the core limits).
    fn session_doc() -> Value {
        json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {
                    "maxCallsInRequest": 16,
                    "maxObjectsInGet": 500,
                    "maxObjectsInSet": 500
                },
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {},
                "urn:ietf:params:jmap:calendars": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c",
                "urn:ietf:params:jmap:calendars": "c"
            },
            "apiUrl": "https://mail.test.local/jmap/",
            "state": "2f72d7c8"
        })
    }

    #[test]
    fn rebases_api_url_onto_connection_base_by_default() {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let session =
            Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
        // The advertised foreign HTTPS origin is replaced by the connection origin.
        assert_eq!(session.api_url(), "http://127.0.0.1:18080/jmap/");
        assert_eq!(session.mail_account_id().unwrap(), "c");
        assert_eq!(session.submission_account_id().unwrap(), "c");
        assert_eq!(session.calendar_account_id().unwrap(), "c");
        assert_eq!(session.state(), Some("2f72d7c8"));
    }

    #[test]
    fn trust_advertised_keeps_the_server_origin() {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let session =
            Session::parse(&session_doc(), &base, SessionUrlPolicy::TrustAdvertised).unwrap();
        assert_eq!(session.api_url(), "https://mail.test.local/jmap/");
    }

    #[test]
    fn reads_capabilities_and_limits() {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let session =
            Session::parse(&session_doc(), &base, SessionUrlPolicy::RebaseToConnection).unwrap();
        let caps = session.capabilities();
        assert!(caps.mail() && caps.submission() && caps.calendars());
        assert_eq!(session.limits().max_objects_in_get, 500);
        assert_eq!(session.limits().max_calls_in_request, 16);
    }

    #[test]
    fn missing_api_url_is_a_session_error() {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let doc = json!({ "capabilities": {}, "primaryAccounts": {} });
        assert!(matches!(
            Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection),
            Err(JmapError::Session(_))
        ));
    }

    #[test]
    fn absent_core_capability_falls_back_to_default_limits() {
        let base = Url::parse("http://127.0.0.1:18080").unwrap();
        let doc = json!({
            "capabilities": { "urn:ietf:params:jmap:mail": {} },
            "primaryAccounts": { "urn:ietf:params:jmap:mail": "c" },
            "apiUrl": "https://mail.test.local/jmap/"
        });
        let session = Session::parse(&doc, &base, SessionUrlPolicy::RebaseToConnection).unwrap();
        assert_eq!(session.limits(), CoreLimits::default());
        assert!(session.mail_account_id().is_ok());
    }
}
