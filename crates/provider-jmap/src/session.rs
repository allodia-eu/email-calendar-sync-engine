//! The JMAP session resource (RFC 8620 §2): capabilities, accounts, API URL, and
//! server limits.
//!
//! Three real-world subtleties this handles:
//!
//! - **The account id is looked up, not assumed.** The JMAP account id (e.g. `"c"`) is whatever the
//!   server assigned and is read from `primaryAccounts` per capability; it is distinct from the
//!   engine's host-assigned [`AccountId`](engine_core::ids::AccountId).
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

use engine_provider::{OverrideSurvival, RsvpControls, WriteGuard};
use reqwest::Url;
use serde_json::Value;

use crate::{error::JmapError, request::capability};

/// What a JMAP RSVP can and cannot control.
///
/// **Silence is ours to give**, unlike on the other server-scheduled transport. JMAP
/// schedules only when the request asks it to — `sendSchedulingMessages`, default `false` —
/// so "answer without telling the organizer" is a per-request choice the adapter can honour
/// verbatim, where an RFC 6638 CalDAV server emits the `REPLY` on its own and a client
/// cannot stop it. This read `false` until #102, which was not a judgement about JMAP but a
/// description of an adapter that never sent the argument at all: it advertised that it
/// could not suppress the reply while in fact suppressing *every* reply.
///
/// The note is not ours. RFC 8984 defines a `participationComment`, but whether a given
/// server carries it into the reply is unverified against any server we run, so it is
/// advertised as absent rather than promised — a note that may go nowhere is worse than one
/// never offered.
///
/// The guard is [`WriteGuard::Absent`] for the same reason every JMAP write is: a
/// `CalendarEvent` carries no per-object revision. Declared once, and used both to advertise
/// and to enforce, so the two can never disagree.
pub(crate) const JMAP_RSVP: RsvpControls = RsvpControls {
    comment: false,
    suppress_notification: true,
    guard: WriteGuard::Absent,
};

/// What a JMAP series edit costs the user: nothing.
///
/// A `/set` takes a `PatchObject`, so an edit names the master's own properties and the
/// `recurrenceOverrides` map is not among them. Measured against Stalwart, and re-measured
/// by `tests/live_calendar_survival.rs`.
pub(crate) const JMAP_OVERRIDE_SURVIVAL: OverrideSurvival = OverrideSurvival::kept();

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
    /// Max requests the server will accept at once (`maxConcurrentRequests`).
    ///
    /// RFC 8620 §2 scopes this to the API endpoint, and defines no companion limit for
    /// the download endpoint — but a server is free to apply one number to both, and
    /// Stalwart does: exceeding it on a blob download is refused with a `400`
    /// `urn:ietf:params:jmap:error:limit`, not queued. So this is what bounds a
    /// concurrent body warm too. It defaults to `1` rather than to a guess, because the
    /// cost of being wrong is asymmetric — too narrow is slow, too wide is refused
    /// requests.
    ///
    /// Reading it rather than picking a number is what makes this right on both servers
    /// seen so far: Stalwart says 4 and enforces it, Fastmail says 10. Measured against a
    /// live Fastmail account over 80 bodies, throughput is linear the whole way up —
    /// 5.4 bodies/s at 1, 21.1 at 4, **48.5 at 10** — so a constant tuned for either server
    /// would be wrong for the other by about a factor of two in one direction or the other.
    ///
    /// Note that Fastmail did not *refuse* a 16-wide drain, which was faster still (63.7).
    /// The advertised number is respected anyway: it is what the server asked for, one of
    /// these two servers enforces it with a hard `400`, and 9× is not worth the refusals.
    pub max_concurrent_requests: usize,
}

impl Default for CoreLimits {
    fn default() -> Self {
        // Conservative RFC-floor-ish fallbacks if the server omits the core
        // capability (it never should). Keeps batching correct, just smaller.
        Self {
            max_objects_in_get: 100,
            max_objects_in_set: 100,
            max_calls_in_request: 16,
            max_concurrent_requests: 1,
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
    mail_account_id: Option<String>,
    submission_account_id: Option<String>,
    calendar_account_id: Option<String>,
    contact_account_id: Option<String>,
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

        let primary = value.get("primaryAccounts");
        let account_for = |urn: &str| {
            primary
                .and_then(|p| p.get(urn))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let mail_account_id = account_for(capability::MAIL);
        let calendar_account_id = account_for(capability::CALENDARS);
        let contact_account_id = account_for(capability::CONTACTS);

        let caps = value.get("capabilities");
        let has = |urn: &str| caps.is_some_and(|c| c.get(urn).is_some());
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
        if capabilities.mail() && !account_is_read_only(value, mail_account_id.as_deref()) {
            capabilities = capabilities.with_mail_writes();
            // Reporting rides the same gate: the report *is* the IANA-registered keyword
            // (`$junk`/`$notjunk`/`$phishing`), and RFC 8621 §4.1.1 makes keywords part of
            // the mail capability, so anything that can write mail can carry all three.
            // The evidence is `Convention` and that is not pessimism: the spec's SHOULD is
            // addressed to *clients*, no capability advertises whether the server trains on
            // the keyword, and a server that ignored it would answer identically. Verified
            // against Stalwart, which stores and returns all three (`crate::report`).
            capabilities = capabilities.with_mail_report(engine_provider::ReportControls {
                verdicts: engine_provider::ReportVerdicts::all(),
                evidence: engine_provider::ReportEvidence::Convention,
            });
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
        if capabilities.calendars() && !account_is_read_only(value, calendar_account_id.as_deref())
        {
            // Scheduling is advertised because the adapter *asks* for it: every calendar
            // verb sends `sendSchedulingMessages` (`crate::calendar_write`), so the server
            // — not the caller — delivers the iTIP. That is what a caller needs to know
            // before deciding whether it must send an iMIP message itself.
            //
            // There is nothing here to detect, and the flag is not claiming otherwise.
            // JMAP Calendars leaves scheduling to the implementation and exposes no
            // capability to probe, so a server that accepted the argument and quietly did
            // nothing would look exactly like one that scheduled. Contrast CalDAV, where
            // RFC 6638 §2 gives a discoverable answer and the adapter discovers it.
            capabilities = capabilities
                .with_calendar_writes(WriteGuard::Absent, JMAP_OVERRIDE_SURVIVAL)
                .with_calendar_rsvp(JMAP_RSVP)
                .with_calendar_scheduling();
        }
        if capabilities.contacts() && !account_is_read_only(value, contact_account_id.as_deref()) {
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

        let limits = caps
            .and_then(|c| c.get(capability::CORE))
            .map(parse_limits)
            .unwrap_or_default();

        Ok(Self {
            api_url,
            download_url,
            upload_url,
            event_source_url,
            mail_account_id,
            submission_account_id: account_for(capability::SUBMISSION),
            calendar_account_id,
            contact_account_id,
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

    /// The JMAP account id for address books and contact cards.
    pub(crate) fn contact_account_id(&self) -> Result<&str, JmapError> {
        self.contact_account_id
            .as_deref()
            .ok_or_else(|| JmapError::session("no primary contacts account"))
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

/// Whether the mail account is read-only (`accounts.<id>.isReadOnly`, RFC 8620 §2).
/// Defaults to writable when the account object or the flag is absent, matching the
/// RFC default (`isReadOnly` is optional and defaults to `false`).
fn account_is_read_only(session: &Value, mail_account_id: Option<&str>) -> bool {
    let Some(id) = mail_account_id else {
        return false;
    };
    session
        .get("accounts")
        .and_then(|accounts| accounts.get(id))
        .and_then(|account| account.get("isReadOnly"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
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
        max_concurrent_requests: read("maxConcurrentRequests", defaults.max_concurrent_requests),
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
