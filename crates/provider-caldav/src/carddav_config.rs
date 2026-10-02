//! CardDAV connection settings.
//!
//! Split from [`crate::carddav`], which had reached the size limit: that module is the adapter
//! a host drives, this one is what it is built from.

use std::sync::Arc;

use engine_provider::ConnectObserver;
use engine_tls::TlsClientConfig;

use crate::transport::Credentials;

/// CardDAV connection settings.
#[derive(Clone)]
pub struct CardDavConfig {
    /// Server origin.
    pub base_url: String,
    /// HTTP authentication.
    pub credentials: Credentials,
    /// Discovery path, normally `/.well-known/carddav`.
    pub discovery_path: String,
    /// Home-relative name or absolute href of the bound address book.
    pub address_book: String,
    /// Shared TLS trust policy.
    pub tls: TlsClientConfig,
    /// Shared throttling policy (`docs/agent-guidance/http-throttling.md`).
    pub retry: engine_http::RetryConfig,
    /// Private, as on `CalDavConfig`: a `dyn` observer is neither `Debug` nor meaningfully
    /// inspectable, so it is set through [`CardDavConfig::with_connect_observer`].
    pub(crate) connect_observer: Option<Arc<dyn ConnectObserver>>,
}

impl core::fmt::Debug for CardDavConfig {
    /// Hand-written because the observer is a `dyn` trait object; the credentials redact
    /// themselves.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CardDavConfig")
            .field("base_url", &self.base_url)
            .field("credentials", &self.credentials)
            .field("discovery_path", &self.discovery_path)
            .field("address_book", &self.address_book)
            .field("tls", &self.tls)
            .field("retry", &self.retry)
            .field("connect_observer", &self.connect_observer.is_some())
            .finish()
    }
}

impl CardDavConfig {
    /// Creates settings bound to the `default` address book.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            discovery_path: "/.well-known/carddav".into(),
            address_book: "default".into(),
            tls: TlsClientConfig::default(),
            retry: engine_http::RetryConfig::default(),
            connect_observer: None,
        }
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
