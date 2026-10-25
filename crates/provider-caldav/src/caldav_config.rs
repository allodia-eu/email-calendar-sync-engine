//! CalDAV connection settings.
//!
//! Split from [`crate::provider`] the way `carddav_config` is from `carddav`: that module is the
//! adapter a host drives, this one is what it is built from.

use std::sync::Arc;

use engine_provider::ConnectObserver;
use engine_tls::TlsClientConfig;

use crate::{credential_hosts::CredentialHosts, transport::Credentials};

/// Connection settings for a CalDAV account.
#[derive(Clone)]
pub struct CalDavConfig {
    /// The server origin, e.g. `https://dav.example.com`.
    pub base_url: String,
    /// How to authenticate.
    pub credentials: Credentials,
    /// The URL to begin discovery at; defaults to the RFC 6764 well-known path.
    pub discovery_path: String,
    /// The calendar collection to bind events to — a name under the calendar home
    /// (e.g. `default`) or an absolute collection path.
    pub calendar: String,
    /// The TLS trust policy for this account, shared with every other provider
    /// (`docs/agent-guidance/tls.md`). Defaults to the hermetic bundled roots;
    /// override with [`CalDavConfig::with_tls`].
    pub tls: TlsClientConfig,
    /// The throttling policy for this account, shared with every other provider
    /// (`docs/agent-guidance/http-throttling.md`). Defaults to waiting a `429` out;
    /// override with [`CalDavConfig::with_retry`].
    pub retry: engine_http::RetryConfig,
    /// The product every request names in its `User-Agent`: the host's application.
    pub user_agent: engine_http::UserAgent,
    /// Private, unlike its siblings: a `dyn` observer is neither `Debug` nor
    /// meaningfully inspectable, so it is set through
    /// [`CalDavConfig::with_connect_observer`] and read only by
    /// [`CalDavProvider::connect`].
    pub(crate) connect_observer: Option<Arc<dyn ConnectObserver>>,
    /// Private for the same reason; set through [`CalDavConfig::with_credential_hosts`].
    pub(crate) credential_hosts: Option<Arc<dyn CredentialHosts>>,
}

impl core::fmt::Debug for CalDavConfig {
    /// Hand-written because the observer is a `dyn` trait object; the credentials
    /// redact themselves (`Credentials`).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CalDavConfig")
            .field("base_url", &self.base_url)
            .field("credentials", &self.credentials)
            .field("discovery_path", &self.discovery_path)
            .field("calendar", &self.calendar)
            .field("tls", &self.tls)
            .field("retry", &self.retry)
            .field("user_agent", &self.user_agent)
            .field("connect_observer", &self.connect_observer.is_some())
            .field("credential_hosts", &self.credential_hosts.is_some())
            .finish()
    }
}

impl CalDavConfig {
    /// Settings with the RFC 6764 well-known discovery path and the `default`
    /// calendar.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        credentials: Credentials,
        user_agent: engine_http::UserAgent,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            discovery_path: "/.well-known/caldav".to_owned(),
            calendar: "default".to_owned(),
            tls: TlsClientConfig::default(),
            retry: engine_http::RetryConfig::default(),
            user_agent,
            connect_observer: None,
            credential_hosts: None,
        }
    }

    /// Lets `policy` decide which hosts receive the credential ([`CredentialHosts`]).
    #[must_use]
    pub fn with_credential_hosts(mut self, policy: Arc<dyn CredentialHosts>) -> Self {
        self.credential_hosts = Some(policy);
        self
    }

    /// Binds events to a different calendar collection (a home-relative name or an
    /// absolute path).
    #[must_use]
    pub fn with_calendar(mut self, calendar: impl Into<String>) -> Self {
        self.calendar = calendar.into();
        self
    }

    /// Starts discovery at `path` instead of `/.well-known/caldav`: a path on the base URL, or
    /// an absolute URL such as a principal the host stored from an earlier connect.
    #[must_use]
    pub fn with_discovery_path(mut self, path: impl Into<String>) -> Self {
        self.discovery_path = path.into();
        self
    }

    /// Sets the TLS trust policy (the host builds one and shares it across the
    /// account's providers).
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Sets the throttling policy (the host builds one and shares it across the account's
    /// providers, like the TLS policy above).
    #[must_use]
    pub fn with_retry(mut self, retry: engine_http::RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Observes the connect phase: one
    /// [`ConnectStep::Redirected`](engine_provider::ConnectStep::Redirected) per hop discovery
    /// follows itself, then [`ConnectStep::Discovered`](engine_provider::ConnectStep::Discovered)
    /// naming the calendar home.
    ///
    /// No TLS step — the version arrives on a response, after the connect phase, so it
    /// lands in `ConnectionInfo::tls_version` instead (`docs/agent-guidance/tls.md`) —
    /// and no auth step, since CalDAV has no discrete authentication exchange;
    /// credentials ride on each `PROPFIND`.
    ///
    /// The observer rides on the config, so a host that rebuilds this provider after a
    /// dropped session observes the redial too. `Arc` so one host observer can be
    /// shared across the account's providers.
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }
}
