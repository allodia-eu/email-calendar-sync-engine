//! CardDAV connection settings.
//!
//! Split from [`crate::carddav`], which had reached the size limit: that module is the adapter
//! a host drives, this one is what it is built from.

use std::sync::Arc;

use engine_provider::{ConnectObserver, CredentialSource, FixedCredential};
use engine_tls::TlsClientConfig;

#[allow(deprecated)]
use crate::credential_hosts::CredentialHosts;
use crate::transport::Credentials;

/// CardDAV connection settings.
#[derive(Clone)]
pub struct CardDavConfig {
    /// Server origin.
    pub base_url: String,
    /// The URL to begin discovery at: the base URL's path, or the RFC 6764 well-known path when
    /// it names only a server.
    pub discovery_path: String,
    /// Home-relative name or absolute href of the bound address book.
    pub address_book: String,
    /// Shared TLS trust policy.
    pub tls: TlsClientConfig,
    /// Shared throttling policy (`docs/agent-guidance/http-throttling.md`).
    pub retry: engine_http::RetryConfig,
    /// The product every request names in its `User-Agent`: the host's application.
    pub user_agent: engine_http::UserAgent,
    /// Private, as on `CalDavConfig`: a `dyn` observer is neither `Debug` nor meaningfully
    /// inspectable, so it is set through [`CardDavConfig::with_connect_observer`].
    pub(crate) connect_observer: Option<Arc<dyn ConnectObserver>>,
    /// Who the account signs in as, asked for its credential on every request that carries it
    /// (`docs/design/credential-source.md`). Private like the observer: a `dyn` source is the
    /// host's, and is set by [`new`](Self::new) or
    /// [`from_credential_source`](Self::from_credential_source).
    pub(crate) source: Arc<dyn CredentialSource>,
    /// Private for the same reason; set through the deprecated
    /// [`CardDavConfig::with_credential_hosts`].
    #[allow(deprecated)]
    pub(crate) credential_hosts: Option<Arc<dyn CredentialHosts>>,
}

impl core::fmt::Debug for CardDavConfig {
    /// Hand-written because the observer is a `dyn` trait object; the source prints what its
    /// own `Debug` does, which a source must keep free of the credential.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CardDavConfig")
            .field("base_url", &self.base_url)
            .field("source", &self.source)
            .field("discovery_path", &self.discovery_path)
            .field("address_book", &self.address_book)
            .field("tls", &self.tls)
            .field("retry", &self.retry)
            .field("user_agent", &self.user_agent)
            .field("connect_observer", &self.connect_observer.is_some())
            .field("credential_hosts", &self.credential_hosts.is_some())
            .finish()
    }
}

impl CardDavConfig {
    /// Creates settings bound to the `default` address book, beginning discovery at `base_url`'s
    /// path, or at the RFC 6764 well-known when it names only a server, signing in with
    /// `credentials`.
    ///
    /// The credential never changes and is never replaced: it is a [`FixedCredential`] under a
    /// subject of its own. A host whose credential changes (an OAuth token, a broker's grant)
    /// uses [`from_credential_source`](Self::from_credential_source) instead of rebuilding the
    /// provider. Either way, the credential goes over plaintext HTTP only when `base_url` and
    /// the request are both on loopback (`http://127.0.0.1:…`, `localhost`, `::1`): a plaintext
    /// server anywhere else fails the connect as `Permanent` before anything is sent, and an
    /// `https` server is never followed onto `http://`.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        credentials: Credentials,
        user_agent: engine_http::UserAgent,
    ) -> Self {
        Self::from_credential_source(
            base_url,
            Arc::new(FixedCredential::unique(credentials.into())),
            user_agent,
        )
    }

    /// Creates settings bound to the `default` address book, beginning discovery at `base_url`'s
    /// path, or at the RFC 6764 well-known when it names only a server, asking `source` for the
    /// credential on every request that carries it.
    ///
    /// The source is asked when a request is about to carry the credential, under the request's
    /// deadline. Its [`permits`](CredentialSource::permits) is checked for the configured server
    /// before anything is sent, and for every origin discovery moves to; a refusal fails the
    /// connect as `Permanent`. A `401` to a request that carried the credential, whose challenge
    /// offers the credential's scheme or none, asks the source once for a replacement and sends
    /// the request once more with it. Everything a connection learns about which origins answer
    /// belongs to this one source.
    #[must_use]
    pub fn from_credential_source(
        base_url: impl Into<String>,
        source: Arc<dyn CredentialSource>,
        user_agent: engine_http::UserAgent,
    ) -> Self {
        let base_url = base_url.into();
        Self {
            discovery_path: crate::href::discovery_start(&base_url, "/.well-known/carddav"),
            base_url,
            source,
            address_book: "default".into(),
            tls: TlsClientConfig::default(),
            retry: engine_http::RetryConfig::default(),
            user_agent,
            connect_observer: None,
            credential_hosts: None,
        }
    }

    /// Lets `policy` decide which hosts receive the credential ([`CredentialHosts`]), beside the
    /// source's [`permits`](CredentialSource::permits): both must agree.
    #[must_use]
    #[deprecated(
        note = "answer `CredentialSource::permits` instead, and build the config with \
                `from_credential_source`; a policy set here is still consulted beside it"
    )]
    #[allow(deprecated)]
    pub fn with_credential_hosts(mut self, policy: Arc<dyn CredentialHosts>) -> Self {
        self.credential_hosts = Some(policy);
        self
    }

    /// Starts discovery at `path` instead of `/.well-known/carddav`: a path on the base URL, or
    /// an absolute URL such as a principal the host stored from an earlier connect.
    #[must_use]
    pub fn with_discovery_path(mut self, path: impl Into<String>) -> Self {
        self.discovery_path = path.into();
        self
    }

    /// Binds a different address book.
    #[must_use]
    pub fn with_address_book(mut self, address_book: impl Into<String>) -> Self {
        self.address_book = address_book.into();
        self
    }

    /// Overrides TLS trust.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }

    /// Overrides the throttling policy.
    #[must_use]
    pub fn with_retry(mut self, retry: engine_http::RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Observes the connect phase: one
    /// [`ConnectStep::Redirected`](engine_provider::ConnectStep::Redirected) per hop discovery
    /// follows itself, [`ConnectStep::Discovered`](engine_provider::ConnectStep::Discovered)
    /// naming the address-book home, then
    /// [`ConnectStep::Negotiated`](engine_provider::ConnectStep::Negotiated) listing the compliance
    /// classes the home advertised.
    ///
    /// As on `CalDavConfig`, no TLS or authentication step: the TLS version arrives on a
    /// response, after the connect phase, and credentials ride on each request.
    #[must_use]
    pub fn with_connect_observer(mut self, observer: Arc<dyn ConnectObserver>) -> Self {
        self.connect_observer = Some(observer);
        self
    }
}
