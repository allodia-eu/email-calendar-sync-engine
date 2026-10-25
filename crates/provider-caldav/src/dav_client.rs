//! The live, `reqwest`-backed [`DavExecutor`]: resolves hrefs against the connection's
//! base, applies the credential where [`CredentialScope`] says it belongs, and answers one
//! authentication challenge per request. It does not follow redirects itself, so discovery
//! can resolve the RFC 6764 well-known redirect and decide what each hop may receive.

use std::sync::Arc;

use async_trait::async_trait;
use engine_http::{Exchange, ObservedConnection, RetryConfig, send_retrying};
use engine_provider::{HttpVersion, TlsVersion};
use engine_tls::TlsClientConfig;
use reqwest::{Client, Method, redirect::Policy};

use crate::{
    credential_hosts::{self, CredentialHosts},
    credential_scope::CredentialScope,
    error::CalDavError,
    href::single_encoded,
    transport::{Credentials, DavExecutor, DavMethod, HttpResponse, Precondition, WriteRequest},
};

/// The live `reqwest`-backed CalDAV transport.
pub(crate) struct DavClient {
    client: Client,
    /// The origin every relative href resolves onto: the configured server until discovery
    /// settles on the collection home ([`DavExecutor::settle_on`]). Behind a lock for that
    /// one move; read once per request afterwards.
    base: std::sync::RwLock<reqwest::Url>,
    credentials: Credentials,
    /// Which origins receive the credential, and when ([`CredentialScope`]).
    scope: CredentialScope,
    /// The host's say over which origins the credential may move to ([`CredentialHosts`]).
    credential_hosts: Option<Arc<dyn CredentialHosts>>,
    /// The HTTP and TLS versions most recently observed — the post-connect facts
    /// `ConnectionInfo` reports. Every response funnels through
    /// [`DavClient::collect`], and the discovery `PROPFIND` that
    /// [`CalDavProvider::connect`](crate::CalDavProvider::connect) performs populates it
    /// before a provider exists. It then keeps tracking: the RFC 6764 well-known `30x`
    /// this client follows *itself* may be a different origin from the calendar home
    /// that serves every real request, so the latest observation — not the first — is
    /// the one that describes the working connection.
    connection: ObservedConnection,
    /// How a `429` is waited out. Note that `PROPFIND` and `REPORT` are extension methods
    /// rather than ones `http` knows are idempotent, so a `503` on them is not retried even
    /// though their own RFCs say it would be safe.
    retry: RetryConfig,
}

impl core::fmt::Debug for DavClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DavClient")
            .field("base", &self.base().as_str())
            .finish_non_exhaustive()
    }
}

impl DavClient {
    /// Builds a transport against `base_url` (the server origin), using the given
    /// credentials. Redirect-following is disabled so discovery handles the
    /// well-known `307` itself.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] if `base_url` is not a valid URL or the HTTP client
    /// cannot be built.
    ///
    /// `tls` carries the host's trust policy (`docs/agent-guidance/tls.md`); the
    /// library builds no trust store of its own.
    pub(crate) fn new(
        base_url: &str,
        credentials: Credentials,
        tls: &TlsClientConfig,
        retry: &RetryConfig,
    ) -> Result<Self, CalDavError> {
        let base = reqwest::Url::parse(base_url)
            .map_err(|e| CalDavError::protocol(format!("bad base URL {base_url:?}: {e}")))?;
        let client = engine_http::client(tls).redirect(Policy::none()).build()?;
        Ok(Self {
            client,
            scope: CredentialScope::new(&base),
            base: std::sync::RwLock::new(base),
            credentials,
            credential_hosts: None,
            connection: ObservedConnection::default(),
            retry: retry.clone().labelled("caldav"),
        })
    }
}

impl DavClient {
    /// Lets `policy` refuse an origin discovery would move the credential to.
    #[must_use]
    pub(crate) fn with_credential_hosts(
        mut self,
        policy: Option<Arc<dyn CredentialHosts>>,
    ) -> Self {
        self.credential_hosts = policy;
        self
    }

    /// The connection's current origin. Cloned rather than borrowed so no request is
    /// built while the lock is held.
    fn base(&self) -> reqwest::Url {
        self.base.read().expect("base lock").clone()
    }

    /// Reduces a finished reqwest response to an [`HttpResponse`], reading its body and
    /// the `Location`/`ETag` headers — and recording the negotiated HTTP and TLS
    /// versions on the way through. The one funnel every read and write response passes,
    /// so no path can forget to observe them.
    ///
    /// On a `write` the status is the server's whole answer, so a body lost after it leaves
    /// the reply as the status says rather than failing it.
    async fn collect(
        &self,
        response: engine_http::Sent,
        write: bool,
    ) -> Result<HttpResponse, CalDavError> {
        self.connection.record(&response);
        let status = response.status().as_u16();
        let header = |name: reqwest::header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let location = header(reqwest::header::LOCATION);
        let etag = header(reqwest::header::ETAG);
        // Fastmail splits `DAV` over several field lines, with `addressbook` after the first, and
        // RFC 9110 §5.3 reads repeated lines as one comma-separated list
        // (`docs/spec-deviations.md`, "Permitted, and easy to get wrong").
        let dav = {
            let lines: Vec<&str> = response
                .headers()
                .get_all("dav")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .collect();
            (!lines.is_empty()).then(|| lines.join(", "))
        };
        // Read before the body consumes the reply: this is the funnel's own note of what
        // the server said about when, and it is the last place it exists.
        let retry_after = response.stated_wait();
        let body = match response.text().await {
            Ok(body) => body,
            Err(_) if write => String::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(HttpResponse {
            status,
            body,
            location,
            etag,
            dav,
            retry_after,
        })
    }

    /// Resolves `href` against the connection's base.
    fn target(&self, href: &str) -> Result<reqwest::Url, CalDavError> {
        self.base()
            .join(href)
            .map_err(|e| CalDavError::protocol(format!("bad href {href:?}: {e}")))
    }

    /// A request for `method` to `url`, carrying the credential when `authenticate`.
    fn request(
        &self,
        method: DavMethod,
        url: reqwest::Url,
        authenticate: bool,
    ) -> Result<reqwest::RequestBuilder, CalDavError> {
        let method = Method::from_bytes(method.as_str().as_bytes())
            .map_err(|e| CalDavError::protocol(format!("bad method: {e}")))?;
        let builder = self.client.request(method, url);
        if !authenticate {
            return Ok(builder);
        }
        Ok(match &self.credentials {
            Credentials::Basic { username, password } => {
                builder.basic_auth(username, Some(password))
            }
            Credentials::Bearer(token) => builder.bearer_auth(token),
        })
    }

    /// Sends `method` to `href`, with whatever `dress` adds and `body`, and answers one
    /// authentication challenge: the shared head of every read and write. The body is `Bytes` so
    /// answering a challenge sends the same buffer again rather than a copy.
    ///
    /// The credential rides along unasked only to an origin the scope already answers
    /// ([`CredentialScope::sends_to`]). An origin that answers `401` with a challenge in the
    /// credential's scheme is asked again with it when it is trusted, and never otherwise, so
    /// a host named by remote content (a vCard photo URL) is not sent the password however it
    /// asks.
    ///
    /// A `400` to a URL with a doubly encoded octet is asked once more with it encoded once
    /// ([`single_encoded`]): a server that lists such an href and then refuses it means the
    /// resource that answers there. A `400` applied nothing, so a write is safe to send again.
    async fn exchange(
        &self,
        method: DavMethod,
        href: &str,
        dress: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        body: Option<bytes::Bytes>,
    ) -> Result<engine_http::Sent, CalDavError> {
        let url = self.target(href)?;
        let response = self
            .exchange_at(method, &url, &dress, body.as_ref())
            .await?;
        if response.status() != reqwest::StatusCode::BAD_REQUEST {
            return Ok(response);
        }
        match single_encoded(&url) {
            Some(once) => self.exchange_at(method, &once, &dress, body.as_ref()).await,
            None => Ok(response),
        }
    }

    /// One [`exchange`](Self::exchange) at `url`, answering one challenge.
    async fn exchange_at(
        &self,
        method: DavMethod,
        url: &reqwest::Url,
        dress: &impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        body: Option<&bytes::Bytes>,
    ) -> Result<engine_http::Sent, CalDavError> {
        let url = url.clone();
        let build = |authenticate| -> Result<reqwest::RequestBuilder, CalDavError> {
            let request = dress(self.request(method, url.clone(), authenticate)?);
            Ok(match body {
                Some(body) => request.body(body.clone()),
                None => request,
            })
        };
        let authenticated = self.scope.sends_to(&url);
        let response =
            send_retrying(build(authenticated)?, &self.retry, Exchange::Ordinary).await?;
        if authenticated || response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        if !self
            .scope
            .answers(&url, response.headers(), &self.credentials)
        {
            // A trusted origin that asked only for a method this credential cannot answer: say so,
            // rather than leave a `401` that reads as the credential being refused.
            if let Some(offered) =
                self.scope
                    .unanswerable(&url, response.headers(), &self.credentials)
            {
                return Err(CalDavError::UnsupportedAuthentication(offered));
            }
            return Ok(response);
        }
        Ok(send_retrying(build(true)?, &self.retry, Exchange::Ordinary).await?)
    }
}

#[async_trait]
impl DavExecutor for DavClient {
    fn http_version(&self) -> Option<HttpVersion> {
        self.connection.http_version()
    }

    fn tls_version(&self) -> Option<TlsVersion> {
        self.connection.tls_version()
    }

    fn absolute(&self, href: &str) -> String {
        self.target(href)
            .map_or_else(|_| href.to_owned(), |url| url.to_string())
    }

    fn sends_credential_to(&self, href: &str) -> bool {
        self.target(href).is_ok_and(|url| self.scope.sends_to(&url))
    }

    fn authenticate_unasked(&self, href: &str) -> bool {
        self.target(href)
            .is_ok_and(|url| self.scope.volunteer(&url))
    }

    fn trust_origin(&self, url: &str) -> bool {
        // A relative href, an opaque origin (`data:`, `blob:`) or an unparseable URL names
        // no host to trust.
        let Ok(next) = reqwest::Url::parse(url) else {
            return true;
        };
        if !next.origin().is_tuple() {
            return true;
        }
        // A connection that started on TLS never trusts a plaintext origin: the password
        // would cross the wire in the clear, to a host nobody typed. A chain that began in
        // plaintext (the loopback fixtures) is left alone.
        if self.base().scheme() == "https" && next.scheme() != "https" {
            return false;
        }
        if !credential_hosts::permitted(self.credential_hosts.as_ref(), next.as_str()) {
            return false;
        }
        self.scope.trust(&next);
        true
    }

    fn settle_on(&self, home: &str) -> String {
        let Ok(url) = reqwest::Url::parse(home) else {
            return home.to_owned();
        };
        let Ok(origin) = reqwest::Url::parse(&url.origin().ascii_serialization()) else {
            return home.to_owned();
        };
        *self.base.write().expect("base lock") = origin;
        match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        }
    }

    async fn send(
        &self,
        method: DavMethod,
        href: &str,
        depth: &str,
        body: String,
    ) -> Result<HttpResponse, CalDavError> {
        let response = self
            .exchange(
                method,
                href,
                |request| {
                    request.header("Depth", depth).header(
                        reqwest::header::CONTENT_TYPE,
                        "application/xml; charset=utf-8",
                    )
                },
                Some(body.into()),
            )
            .await?;
        self.collect(response, false).await
    }

    async fn send_options(&self, href: &str) -> Result<HttpResponse, CalDavError> {
        let response = self
            .exchange(DavMethod::Options, href, |request| request, None)
            .await?;
        self.collect(response, false).await
    }

    async fn get_bytes(&self, href: &str) -> Result<Vec<u8>, CalDavError> {
        let response = self
            .exchange(DavMethod::Get, href, |request| request, None)
            .await?;
        self.connection.record(&response);
        let status = response.status().as_u16();
        let wait = response.stated_wait();
        let bytes = response.bytes().await?;
        if (200..300).contains(&status) {
            Ok(bytes)
        } else {
            Err(
                CalDavError::status(status, String::from_utf8_lossy(&bytes).into_owned())
                    .with_retry_after(wait),
            )
        }
    }

    async fn send_write(&self, request: WriteRequest) -> Result<HttpResponse, CalDavError> {
        // A write is ordinary, a scheduling one included: an iTIP message a `PUT` sends is the
        // server's to send, and the precondition the write carries is what keeps a replay
        // from sending it twice (`docs/agent-guidance/deadlines.md`). A `401` challenge
        // applied nothing, so answering it is not a replay either.
        let WriteRequest {
            method,
            href,
            content_type,
            precondition,
            body,
        } = request;
        let response = self
            .exchange(
                method,
                &href,
                |mut builder| {
                    if let Some(content_type) = content_type {
                        builder = builder.header(reqwest::header::CONTENT_TYPE, content_type);
                    }
                    match &precondition {
                        // RFC 7232: `If-None-Match: *` admits only a create; `If-Match` admits
                        // a replace/delete only while the entity tag is unchanged.
                        Precondition::IfNoneMatch => {
                            builder.header(reqwest::header::IF_NONE_MATCH, "*")
                        }
                        Precondition::IfMatch(etag) => {
                            builder.header(reqwest::header::IF_MATCH, etag.as_str())
                        }
                        Precondition::IfScheduleTagMatch(tag) => {
                            builder.header("If-Schedule-Tag-Match", tag.as_str())
                        }
                        Precondition::None => builder,
                    }
                },
                Some(body.into()),
            )
            .await
            .map_err(CalDavError::after_write)?;
        self.collect(response, true).await
    }
}

// The live `DavClient` tests need a mock HTTP server; they live in a sibling file so
// this one stays under the line limit.
#[cfg(test)]
#[path = "transport_tests.rs"]
mod http_transport_tests;

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod deadline_tests;
