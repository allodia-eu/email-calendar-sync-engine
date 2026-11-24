//! Building a [`DavClient`] for an account, and what its credential may be sent to.

use std::sync::Arc;

use engine_http::{AuthenticatedClient, ObservedConnection, RetryConfig};
use engine_provider::{CredentialSource, FixedCredential};
use engine_tls::TlsClientConfig;

use super::DavClient;
#[allow(deprecated)]
use crate::credential_hosts::{self, CredentialHosts};
use crate::{credential_scope::CredentialScope, error::CalDavError, transport::Credentials};

impl DavClient {
    /// A transport against `base_url` with a fixed credential under a subject of its own: the
    /// tests' and examples' shorthand for [`from_source`](Self::from_source).
    ///
    /// # Errors
    ///
    /// As [`from_source`](Self::from_source).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        base_url: &str,
        credentials: Credentials,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, CalDavError> {
        let source = Arc::new(FixedCredential::unique(credentials.into()));
        Self::from_source(base_url, source, tls, retry, user_agent)
    }

    /// Builds a transport against `base_url` (the server origin), asking `source` for the
    /// credential on each request that carries it. Redirect-following is disabled so discovery
    /// handles the well-known `307` itself.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if `base_url` is not a valid URL or the HTTP client
    /// cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`); the
    /// library builds no trust store of its own.
    pub(crate) fn from_source(
        base_url: &str,
        source: Arc<dyn CredentialSource>,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
        user_agent: &engine_http::UserAgent,
    ) -> Result<Self, CalDavError> {
        let base = reqwest::Url::parse(base_url)
            .map_err(|e| CalDavError::protocol(format!("bad base URL {base_url:?}: {e}")))?;
        let retry = retry
            .clone()
            .labelled("caldav")
            // Stalwart refuses a request over its concurrency cap with a `400`, which the
            // shared status rule reads as a request defect. See `DavThrottles`.
            .classifying(Arc::new(crate::throttle::DavThrottles));
        Ok(Self {
            http: AuthenticatedClient::new(tls, user_agent, base.clone(), source, retry.clone())?,
            scope: CredentialScope::new(&base),
            base: std::sync::RwLock::new(base),
            credential_hosts: None,
            connection: ObservedConnection::default(),
            retry,
        })
    }
}

impl DavClient {
    /// Lets `policy` refuse an origin discovery would move the credential to.
    #[must_use]
    #[allow(deprecated)]
    pub(crate) fn with_credential_hosts(
        mut self,
        policy: Option<Arc<dyn CredentialHosts>>,
    ) -> Self {
        self.credential_hosts = policy;
        self
    }

    /// Whether `url`'s origin may ever receive the credential: the engine's plaintext rule and
    /// the source's `permits` ([`AuthenticatedClient::admits`]), and the deprecated host policy
    /// where one is set. Discovery asks it of its start before anything is sent
    /// (`discovery::trusted_start`), so a configured server that may not receive the credential
    /// is never dialled, and of every origin it moves to.
    pub(super) fn admits(&self, url: &reqwest::Url) -> bool {
        self.http.admits(url)
            && credential_hosts::permitted(self.credential_hosts.as_ref(), url.as_str())
    }
}
