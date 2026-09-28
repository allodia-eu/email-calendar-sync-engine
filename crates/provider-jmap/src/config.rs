//! How to connect a [`JmapClient`](crate::JmapClient): the credential it presents and the
//! connection it makes.
//!
//! Split from the crate root, which keeps the client itself.

use core::fmt;
use std::sync::Arc;

use engine_core::ids::SharedMailboxId;
use engine_provider::ConnectObserver;
use engine_tls::TlsClientConfig;

use crate::SessionUrlPolicy;

/// Credentials for authenticating to a JMAP server.
///
/// This names the credential the caller *holds*, not the header that goes on the wire.
/// JMAP specifies no authentication mechanism of its own — RFC 8620 §8.2 defers to the
/// IANA scheme registry and marks Basic NOT RECOMMENDED — so the scheme is whatever the
/// server challenges for, and the transport re-frames the secret to match (see
/// `crate::auth`). A `Basic` credential is therefore also presentable as a bearer token;
/// the variant records that a username came with it, not that Basic will be sent.
///
/// `Debug` is redacted — the secret never appears in logs (`north-star.md` security).
#[derive(Clone)]
pub enum Credentials {
    /// A username and secret: a login password, an app-specific password, or an API
    /// token that the user happened to enter alongside their address.
    Basic {
        /// The username (full email address for the fixture).
        username: String,
        /// The password or app-specific token.
        password: String,
    },
    /// A bare secret with no username — an OAuth or API bearer token.
    Bearer(String),
}

impl Credentials {
    /// HTTP Basic credentials.
    #[must_use]
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::Basic {
            username: username.into(),
            password: password.into(),
        }
    }

    /// An OAuth bearer token.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Self::Bearer(token.into())
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render the secret.
        let kind = match self {
            Self::Basic { username, .. } => format!("Basic {{ username: {username:?}, .. }}"),
            Self::Bearer(_) => "Bearer(..)".to_owned(),
        };
        f.write_str(&kind)
    }
}

/// How to connect a [`JmapClient`](crate::JmapClient).
#[derive(Clone)]
pub struct JmapConfig {
    pub(crate) base_url: String,
    pub(crate) credentials: Credentials,
    pub(crate) session_path: String,
    pub(crate) session_urls: SessionUrlPolicy,
    pub(crate) tls: TlsClientConfig,
    pub(crate) retry: engine_http::RetryConfig,
    pub(crate) connect_observer: Option<Arc<dyn ConnectObserver>>,
    pub(crate) shared_mailbox: Option<SharedMailboxId>,
}

impl JmapConfig {
    /// Configures a connection to `base_url` (e.g. `http://127.0.0.1:18080`) with
    /// `credentials`, defaulting to well-known session discovery and rebasing
    /// advertised URLs onto the connection.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            session_path: "/.well-known/jmap".to_owned(),
            session_urls: SessionUrlPolicy::RebaseToConnection,
            tls: TlsClientConfig::default(),
            retry: engine_http::RetryConfig::default(),
            connect_observer: None,
            shared_mailbox: None,
        }
    }

    /// Binds this client to a mailbox shared **with** the credential rather than to the
    /// credential's own: `handle` is what
    /// [`list_shared_mailboxes`](engine_provider::Provider::list_shared_mailboxes) or
    /// [`resolve_shared_mailbox`](engine_provider::Provider::resolve_shared_mailbox) handed
    /// back.
    ///
    /// Every method call then carries that account's id instead of the `primaryAccounts`
    /// entry, and each domain is advertised only where the shared account itself exposes it
    /// (`crate::session_accounts`). A host dials one client per account, as it does for any
    /// two mailboxes — which is the whole of what onboarding a shared mailbox costs here.
    ///
    /// The handle is checked at connect: one the session does not list (access withdrawn,
    /// or a handle from another server) fails [`JmapClient::connect`](crate::JmapClient::connect)
    /// rather than every call after it.
    #[must_use]
    pub fn with_shared_mailbox(mut self, handle: SharedMailboxId) -> Self {
        self.shared_mailbox = Some(handle);
        self
    }

    /// Overrides the session-discovery path (default `/.well-known/jmap`).
    #[must_use]
    pub fn with_session_path(mut self, path: impl Into<String>) -> Self {
        self.session_path = path.into();
        self
    }

    /// Overrides how advertised session URLs are resolved.
    #[must_use]
    pub fn with_session_urls(mut self, policy: SessionUrlPolicy) -> Self {
        self.session_urls = policy;
        self
    }

    /// Sets the TLS trust policy (the host builds one and shares it across the
    /// account's providers). Defaults to the hermetic bundled roots
    /// (`docs/agent-guidance/tls.md`).
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Sets the throttling policy (the host builds one and shares it across the account's
    /// providers, like the TLS policy above). Defaults to waiting a `429` out
    /// (`docs/agent-guidance/http-throttling.md`).
    #[must_use]
    pub fn with_retry(mut self, retry: engine_http::RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Observes the connect phase: one
    /// [`ConnectStep::Redirected`](engine_provider::ConnectStep::Redirected) per well-known hop,
    /// [`ConnectStep::Authenticated`](engine_provider::ConnectStep::Authenticated) when the server
    /// serves the session, and
    /// [`ConnectStep::Discovered`](engine_provider::ConnectStep::Discovered) naming the `apiUrl`
    /// that will serve every method call. No TLS step — an HTTP adapter learns its TLS version
    /// from a response, after the connect phase, so it lands in `ConnectionInfo::tls_version`
    /// instead (`docs/agent-guidance/tls.md`).
    ///
    /// The observer rides on the config, so a host that rebuilds this client after a
    /// dropped session observes the redial too. `Arc` so one host observer can be
    /// shared across the account's providers.
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }
}

impl fmt::Debug for JmapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JmapConfig")
            .field("base_url", &self.base_url)
            .field("session_path", &self.session_path)
            .field("session_urls", &self.session_urls)
            .field("shared_mailbox", &self.shared_mailbox)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
