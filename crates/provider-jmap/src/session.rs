//! The JMAP session resource (RFC 8620 §2): capabilities, accounts, API URL, and
//! server limits.
//!
//! Three real-world subtleties this handles:
//!
//! - **The account id is looked up, not assumed.** The JMAP account id (e.g. `"c"`) is whatever the
//!   server assigned and is read from `primaryAccounts` per capability — or from the account a
//!   provider is *bound* to, when it opens a store shared with the credential rather than its own
//!   (`session_accounts`). Either way it is distinct from the engine's host-assigned
//!   [`AccountId`](engine_core::ids::AccountId).
//! - **The advertised `apiUrl` may point at a different origin** than the one the
//!   client connected to (Stalwart advertises its configured public host,
//!   `https://mail.test.local/`, while tests connect to `127.0.0.1:18080`). The
//!   [`SessionUrlPolicy`] decides whether to trust the advertised origin or rebase
//!   it onto the connection base — the safe default for proxied / self-hosted /
//!   test setups.
//! - **A session may span more than one origin, on purpose.** Fastmail serves its `apiUrl` from
//!   `api.fastmail.com` but its `downloadUrl` from `www.fastmailusercontent.com`, a separate
//!   cookie-less origin for untrusted user content. Rebasing is therefore scoped to the session's
//!   *own* advertised origin: a URL the server deliberately puts elsewhere is left alone
//!   (`rebase_template`).

use engine_provider::{SharedMailboxes, WriteGuard};
use reqwest::Url;
use serde_json::Value;

use crate::{
    error::JmapError,
    request::capability,
    session_accounts::{self, SessionAccount},
};

/// How to resolve the session's advertised URLs against the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUrlPolicy {
    /// Replace the advertised origin (scheme/host/port) with the connection base,
    /// keeping only the path. Correct for reverse-proxied, self-hosted, and test
    /// servers that advertise a public hostname they are not reached at. Applies only
    /// to URLs on the session's **own** advertised origin — an endpoint the server
    /// deliberately serves cross-origin is kept verbatim (`rebase_template`).
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
    download_url: Option<String>,
    upload_url: Option<String>,
    event_source_url: Option<String>,
    /// Every account the credential can reach (RFC 8620 §1.6.2), in document order — its
    /// own plus each one shared with it. See `session_accounts`.
    accounts: Vec<SessionAccount>,
    /// The account the four `*_account_id()` accessors resolve to, when a provider is
    /// bound to one rather than to the credential's own store.
    selected_account: Option<String>,
    primary_mail_account_id: Option<String>,
    primary_submission_account_id: Option<String>,
    primary_calendar_account_id: Option<String>,
    primary_contact_account_id: Option<String>,
    limits: CoreLimits,
    capabilities: engine_provider::Capabilities,
    state: Option<String>,
}

impl Session {
    /// Parses the session document, resolving its URLs against `base` per `policy` and
    /// binding method calls to `selected_account` when one is named.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if `apiUrl` is absent or unparseable, or if
    /// `selected_account` is not one of the accounts the session lists.
    pub(crate) fn parse(
        value: &Value,
        base: &Url,
        policy: SessionUrlPolicy,
        selected_account: Option<&str>,
    ) -> Result<Self, JmapError> {
        let advertised_api = value
            .get("apiUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| JmapError::session("apiUrl missing"))?;
        let api_url = resolve_against(base, advertised_api, policy)?;

        // The download/upload/event-source URLs are URI *templates*
        // (`{accountId}`/`{blobId}`/…, RFC 8620 §2), so they are rebased origin-only —
        // running the braces through URL parsing (as `resolve_against` does) would
        // percent-encode them. The rebase is scoped to the origin the session advertised
        // for *itself*, so a deliberately cross-origin endpoint survives; see
        // [`rebase_template`].
        let session_origin = origin_of(advertised_api);
        let template = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(|url| rebase_template(base, url, policy, session_origin))
        };
        let download_url = template("downloadUrl");
        let upload_url = template("uploadUrl");
        let event_source_url = template("eventSourceUrl");

        let accounts = session_accounts::parse_accounts(value);
        session_accounts::validate_selection(&accounts, selected_account)?;

        let primary = value.get("primaryAccounts");
        let account_for = |urn: &str| {
            primary
                .and_then(|p| p.get(urn))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let primary_mail_account_id = account_for(capability::MAIL);
        let primary_calendar_account_id = account_for(capability::CALENDARS);
        let primary_contact_account_id = account_for(capability::CONTACTS);

        let caps = value.get("capabilities");
        // A domain is usable only if the *server* advertises it and the account the calls
        // will address exposes it. Unbound those are the same question; bound to a share
        // they are not — a shared mailbox may expose mail and nothing else — so the
        // advertised capability set is the intersection rather than the server's alone.
        let selected = selected_account
            .and_then(|id| accounts.iter().find(|account| account.id == id))
            .cloned();
        let has = |urn: &str| {
            caps.is_some_and(|c| c.get(urn).is_some())
                && selected
                    .as_ref()
                    .is_none_or(|account| account.supports(urn))
        };
        let mut capabilities = build_capabilities(has);
        // On-demand raw-source fetch (Tier-3 bodies) works whenever the server
        // exposes mail and a download template — see [`crate::fetch::message_source`].
        if capabilities.mail() && download_url.is_some() {
            capabilities = capabilities.with_message_source();
        }
        // Mail writes (`Email/set`) work whenever the account exposes mail and is not
        // read-only. RFC 8621 makes `Email/set` part of the mail capability itself;
        // the only server-side gate is the account's `isReadOnly` flag (RFC 8620
        // §2). A read-only account that is somehow written anyway rejects the set with
        // a `forbidden` `SetError` (→ `Permanent`), so a mis-advertisement is safe.
        // Read-only is asked of the account the calls will actually address — the bound
        // share, not the primary — since that is the one the server would refuse.
        let read_only = |primary: Option<&str>| {
            selected_account
                .or(primary)
                .and_then(|id| accounts.iter().find(|account| account.id == id))
                .is_some_and(|account| account.read_only)
        };
        if capabilities.mail() && !read_only(primary_mail_account_id.as_deref()) {
            capabilities = capabilities.with_mail_writes();
        }
        // Calendar writes (`CalendarEvent/set`) work on the same terms — RFC 8621/8984 make
        // `set` part of the calendars capability, and `isReadOnly` on the *calendar* account
        // is the only gate. The guard is `Absent`, and that is the honest answer, not a
        // shortcut: a `CalendarEvent` carries no per-object revision to guard with, and the
        // only precondition RFC 8620 §5.3 offers (`ifInState`) is scoped to the account's
        // whole event state rather than the object — so it would reject a write because an
        // *unrelated* event changed. Stalwart does not enforce it either
        // (`crate::calendar_write`). A host that must detect a concurrent edit on this
        // transport has to do it above the engine, and `calendar_write_guard` is what tells
        // it so before it writes.
        if capabilities.calendars() && !read_only(primary_calendar_account_id.as_deref()) {
            capabilities = capabilities.with_calendar_writes(WriteGuard::Absent);
        }
        if capabilities.contacts() && !read_only(primary_contact_account_id.as_deref()) {
            capabilities = capabilities.with_contact_writes(WriteGuard::Absent);
        }
        if capabilities.contacts() && download_url.is_some() {
            capabilities = capabilities.with_contact_photos();
        }
        // Push (change notification) works whenever the server advertises an
        // EventSource endpoint (RFC 8620 §7.3) *and* the account exposes a domain the
        // engine can watch (mail or calendars) — otherwise a `Changed` could never map
        // to a synced scope. Gated on a syncable domain like the other capabilities,
        // not on the transport alone. See [`crate::watch::JmapWatcher`].
        if event_source_url.is_some() && (capabilities.mail() || capabilities.calendars()) {
            capabilities = capabilities.with_idle();
        }
        // Shared-mailbox discovery *is* the accounts map, so the mechanism is present
        // whenever the server serves one. Gated on it being non-empty rather than assumed
        // from the protocol: a server that omits the map (against RFC 8620 §1.6.2) would
        // otherwise advertise an enumeration that can only ever return nothing.
        if !accounts.is_empty() {
            capabilities = capabilities.with_shared_mailboxes(SharedMailboxes::Enumerable);
        }

        let limits = caps
            .and_then(|c| c.get(capability::CORE))
            .map(parse_limits)
            .unwrap_or_default();

        Ok(Self {
            api_url,
            download_url,
            upload_url,
            event_source_url,
            accounts,
            selected_account: selected_account.map(str::to_owned),
            primary_mail_account_id,
            primary_submission_account_id: account_for(capability::SUBMISSION),
            primary_calendar_account_id,
            primary_contact_account_id,
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

    /// The connection-resolved blob **download** URI template (RFC 8620 §2), with
    /// its `{accountId}`/`{blobId}`/`{type}`/`{name}` placeholders intact, or
    /// `None` if the server advertised none. The provider substitutes the
    /// placeholders to fetch a message's raw source
    /// (`crate::fetch::message_source`).
    pub(crate) fn download_url(&self) -> Option<&str> {
        self.download_url.as_deref()
    }

    /// The connection-resolved blob **upload** URI template (RFC 8620 §6.1), with
    /// its `{accountId}` placeholder intact, or `None` if the server advertised none.
    /// The provider substitutes the placeholder to upload a draft attachment's bytes
    /// before referencing the returned `blobId` in an `Email/set` (`crate::submit`).
    pub(crate) fn upload_url(&self) -> Option<&str> {
        self.upload_url.as_deref()
    }

    /// The connection-resolved **EventSource** URI template (RFC 8620 §7.3), with its
    /// `{types}`/`{closeafter}`/`{ping}` placeholders intact, or `None` if the server
    /// advertised no push endpoint. [`crate::watch::JmapWatcher`] substitutes the
    /// placeholders to open the change-notification stream.
    pub(crate) fn event_source_url(&self) -> Option<&str> {
        self.event_source_url.as_deref()
    }

    /// The JMAP account id mail method calls address (the server's id, not the engine's):
    /// the account this session is **bound** to, or the `primaryAccounts` entry.
    ///
    /// # Errors
    ///
    /// Returns [`JmapError::Session`] if the server advertised no mail account, or if the
    /// bound account does not expose mail.
    pub(crate) fn mail_account_id(&self) -> Result<&str, JmapError> {
        self.account_id(capability::MAIL, "mail")
    }

    /// The JMAP account id for submission (`Identity`/`EmailSubmission`).
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for submission.
    pub(crate) fn submission_account_id(&self) -> Result<&str, JmapError> {
        self.account_id(capability::SUBMISSION, "submission")
    }

    /// The JMAP account id for calendars (`Calendar`/`CalendarEvent`).
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for calendars.
    pub(crate) fn calendar_account_id(&self) -> Result<&str, JmapError> {
        self.account_id(capability::CALENDARS, "calendar")
    }

    /// The JMAP account id for address books and contact cards.
    ///
    /// # Errors
    ///
    /// As [`mail_account_id`](Self::mail_account_id), for contacts.
    pub(crate) fn contact_account_id(&self) -> Result<&str, JmapError> {
        self.account_id(capability::CONTACTS, "contacts")
    }

    /// Every account the credential can reach, its own included — what
    /// [`Provider::list_shared_mailboxes`](engine_provider::Provider::list_shared_mailboxes)
    /// hands back.
    pub(crate) fn accounts(&self) -> &[SessionAccount] {
        &self.accounts
    }

    /// Resolves the account id for one capability through the selector.
    fn account_id(&self, urn: &str, domain: &str) -> Result<&str, JmapError> {
        let primary = match urn {
            capability::SUBMISSION => self.primary_submission_account_id.as_deref(),
            capability::CALENDARS => self.primary_calendar_account_id.as_deref(),
            capability::CONTACTS => self.primary_contact_account_id.as_deref(),
            _ => self.primary_mail_account_id.as_deref(),
        };
        session_accounts::resolve(
            &self.accounts,
            self.selected_account.as_deref(),
            primary,
            urn,
            domain,
        )
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

/// Rebases a URI *template*'s origin onto the connection `base` per `policy`,
/// preserving its path and query verbatim so RFC 6570 placeholders (`{accountId}`,
/// `{blobId}`, …) survive. Unlike [`resolve_against`], it never runs the template
/// through URL parsing — which would percent-encode the `{`/`}` braces and break
/// the later placeholder substitution.
///
/// Under [`SessionUrlPolicy::RebaseToConnection`] the rewrite is scoped to
/// `session_origin` — the origin the session advertised for **itself** (its `apiUrl`).
/// A template the server deliberately serves from a *different* origin is kept verbatim:
/// the mismatch the rebase corrects (a reverse-proxied or self-hosted server advertising
/// a public hostname it is not reached at) applies uniformly to that server's own origin
/// and cannot explain a second one, so rewriting it can only produce a URL the connection
/// host does not route. Fastmail is the live case — its `apiUrl` is on `api.fastmail.com`
/// while `downloadUrl` is on `www.fastmailusercontent.com`, a separate cookie-less origin
/// for untrusted user content — and rebasing it turned every message-source download into
/// a catch-all `302` to a marketing page.
fn rebase_template(
    base: &Url,
    advertised: &str,
    policy: SessionUrlPolicy,
    session_origin: Option<&str>,
) -> String {
    if policy == SessionUrlPolicy::TrustAdvertised {
        return advertised.to_owned();
    }
    let advertised_origin = origin_of(advertised);
    let same_origin = match (advertised_origin, session_origin) {
        // Origin-free: already relative, so it is the session's own origin by definition.
        (None, _) => true,
        (Some(origin), Some(session)) => origin.eq_ignore_ascii_case(session),
        (Some(_), None) => false,
    };
    if !same_origin {
        return advertised.to_owned();
    }
    let path_and_query = advertised_origin.map_or(advertised, |origin| &advertised[origin.len()..]);
    format!(
        "{}/{}",
        base.origin().ascii_serialization(),
        path_and_query.trim_start_matches('/')
    )
}

/// The `scheme://authority` prefix of an absolute URL, or `None` when it carries no
/// scheme (a relative reference). The origin never contains an RFC 6570 placeholder, so
/// splitting at the first `/` after `://` is safe on a URI template too.
fn origin_of(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.find('/').unwrap_or(rest.len());
    Some(&url[..url.len() - rest.len() + authority])
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
    if has(capability::CONTACTS) {
        caps = caps.with_contacts().with_contact_groups();
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
#[path = "session_tests.rs"]
mod tests;
