//! The live, `reqwest`-backed [`DavExecutor`]: resolves hrefs against the connection's
//! base, applies the credential where [`CredentialScope`] says it belongs, and answers one
//! authentication challenge per request. It does not follow redirects itself, so discovery
//! can resolve the RFC 6764 well-known redirect and decide what each hop may receive.
//!
//! The credential is never held. Every request that carries it goes through the account's
//! [`AuthenticatedClient`], which asks the account's
//! [`CredentialSource`](engine_provider::CredentialSource) for it then, refuses an origin the
//! engine's plaintext rule or the source does not admit, and on a `401` to a request that carried
//! it asks the source once for a replacement and sends the request once more
//! (`docs/design/credential-source.md`).

use std::sync::Arc;

use async_trait::async_trait;
use engine_http::{AuthenticatedClient, Exchange, ObservedConnection, RetryConfig, send_retrying};
use engine_provider::{CredentialFailure, HttpVersion, TlsVersion};
use reqwest::Method;

#[allow(deprecated)]
use crate::credential_hosts::CredentialHosts;
use crate::{
    credential_scope::CredentialScope,
    delete_guard::refused_before_delete,
    error::CalDavError,
    href::single_encoded,
    presentation,
    transport::{DavExecutor, DavMethod, HttpResponse, Precondition, WriteRequest},
};

#[path = "dav_client_build.rs"]
mod build;

/// Whether `response` is a concurrency refusal the send funnel claimed and gave up waiting on
/// (`DavThrottles` buffers every `400`, so its body is there to read).
fn is_throttle(response: &engine_http::Sent) -> bool {
    response
        .read_body()
        .and_then(|body| core::str::from_utf8(body).ok())
        .is_some_and(crate::throttle::is_concurrency_limit)
}

/// The live `reqwest`-backed CalDAV transport.
pub(crate) struct DavClient {
    /// The account's client: every request goes out on it, and every one that carries the
    /// credential goes through its [`send`](AuthenticatedClient::send).
    http: AuthenticatedClient,
    /// The origin every relative href resolves onto: the configured server until discovery
    /// settles on the collection home ([`DavExecutor::settle_on`]). Behind a lock for that
    /// one move; read once per request afterwards.
    base: std::sync::RwLock<reqwest::Url>,
    /// Which origins receive the credential, and when ([`CredentialScope`]).
    scope: CredentialScope,
    /// The host's older say over which hosts the credential may move to, kept beside the
    /// source's [`permits`](CredentialSource::permits) while hosts move off it: both must agree.
    #[allow(deprecated)]
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
    /// How a `429`, or Stalwart's `400` concurrency refusal, is waited out. Note that `PROPFIND`
    /// and `REPORT` are extension methods rather than ones `http` knows are idempotent, so a
    /// `503` on them is not retried even though their own RFCs say it would be safe.
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

    /// A request for `method` to `url`, without the credential: [`AuthenticatedClient::send`]
    /// adds it where it goes.
    fn request(
        &self,
        method: DavMethod,
        url: &reqwest::Url,
    ) -> Result<reqwest::RequestBuilder, CalDavError> {
        let method = Method::from_bytes(method.as_str().as_bytes())
            .map_err(|e| CalDavError::protocol(format!("bad method: {e}")))?;
        Ok(self.http.request(method, url.as_str()))
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
    /// With `respell`, a `400` to a URL with a doubly encoded octet is asked once more with it
    /// encoded once ([`single_encoded`]): a server that lists such an href and then refuses it
    /// means the resource that answers there. A `400` applied nothing, so a write is safe to send
    /// again. The once-encoded URL names another resource on a server that meant the `%25`, so a
    /// write is respelled only when it carries a guard, which that other resource refuses.
    async fn exchange(
        &self,
        method: DavMethod,
        href: &str,
        dress: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        body: Option<bytes::Bytes>,
        respell: bool,
    ) -> Result<engine_http::Sent, CalDavError> {
        let url = self.target(href)?;
        let response = self
            .exchange_at(method, &url, &dress, body.as_ref())
            .await?;
        if !respell
            || response.status() != reqwest::StatusCode::BAD_REQUEST
            || is_throttle(&response)
        {
            return Ok(response);
        }
        let Some(once) = single_encoded(&url) else {
            return Ok(response);
        };
        // Released before the second request, so its connection is not held open by an
        // unread body.
        drop(response);
        self.exchange_at(method, &once, &dress, body.as_ref()).await
    }

    /// One [`exchange`](Self::exchange) at `url`, answering one challenge.
    ///
    /// (A `400` the send funnel waited on and gave up is a refusal of *this* URL's request
    /// for now, not of its spelling, so [`exchange`](Self::exchange) hands it back unrespelled:
    /// asking the once-encoded URL would answer for another resource and lose the throttle.)
    ///
    /// An origin the scope already answers is sent the credential with the request. Any other
    /// gets the request without it, and a `401` from a trusted one is answered: the credential is
    /// asked for then, and sent if the challenge offers its scheme. A challenge only for schemes
    /// it cannot answer is [`CalDavError::UnsupportedAuthentication`], with nothing sent, and
    /// without asking the source when no credential HTTP carries could answer it.
    async fn exchange_at(
        &self,
        method: DavMethod,
        url: &reqwest::Url,
        dress: &impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
        body: Option<&bytes::Bytes>,
    ) -> Result<engine_http::Sent, CalDavError> {
        let build = || -> Result<reqwest::RequestBuilder, CalDavError> {
            let request = dress(self.request(method, url)?);
            Ok(match body {
                Some(body) => request.body(body.clone()),
                None => request,
            })
        };
        if self.scope.sends_to(url) {
            return self
                .http
                .send(
                    build()?,
                    Exchange::Ordinary,
                    presentation::present,
                    engine_http::judge_by_scheme,
                )
                .await;
        }
        let response = send_retrying(build()?, &self.retry, Exchange::Ordinary).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED || !self.scope.trusts(url) {
            return Ok(response);
        }
        // A trusted origin asked. Its challenge is kept and its reply let go, so the connection is
        // free for the answer.
        let challenge = response.headers().clone();
        drop(response);
        // A method no credential HTTP carries can answer (`Digest`, `Negotiate`) is said before
        // the source is asked: no token is fetched for a server no account signs in to, and a
        // source that is down cannot turn that verdict into a retry.
        let offered = engine_http::offered_schemes(&challenge);
        if presentation::unanswerable_by_any(&offered) {
            return Err(CalDavError::UnsupportedAuthentication(offered.join(", ")));
        }
        self.http
            .send(
                build()?,
                Exchange::Ordinary,
                |builder, credential| {
                    // A kind HTTP cannot carry has no scheme, and `present` refuses it.
                    if let Some(scheme) = engine_http::credential_scheme(credential) {
                        // A trusted origin that asked only for the method this kind of
                        // credential is not (Basic, for a token): say so, rather than leave a
                        // `401` that reads as the credential being refused.
                        if let Some(offered) = self.scope.unanswerable(url, &challenge, scheme) {
                            return Err(CalDavError::UnsupportedAuthentication(offered));
                        }
                        self.scope.answers(url, &challenge, scheme);
                    }
                    presentation::present(builder, credential)
                },
                engine_http::judge_by_scheme,
            )
            .await
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

    fn trust_origin(&self, url: &str) -> Result<(), CredentialFailure> {
        // A relative href, an opaque origin (`data:`, `blob:`) or an unparseable URL names
        // no host to trust.
        let Ok(next) = reqwest::Url::parse(url) else {
            return Ok(());
        };
        if !next.origin().is_tuple() {
            return Ok(());
        }
        // The engine's plaintext rule and the source's `permits`, and the deprecated host policy
        // where one is set: a connection that started on TLS never trusts a plaintext origin, so
        // the password never crosses the wire in the clear to a host nobody typed, and only an
        // account configured on loopback trusts plaintext loopback (the harnesses).
        self.admission(&next)?;
        self.scope.trust(&next);
        Ok(())
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
                true,
            )
            .await?;
        self.collect(response, false).await
    }

    async fn send_options(&self, href: &str) -> Result<HttpResponse, CalDavError> {
        let response = self
            .exchange(DavMethod::Options, href, |request| request, None, true)
            .await?;
        self.collect(response, false).await
    }

    async fn get_bytes(&self, href: &str) -> Result<Vec<u8>, CalDavError> {
        let response = self
            .exchange(DavMethod::Get, href, |request| request, None, true)
            .await?;
        self.connection.record(&response);
        let status = response.status().as_u16();
        let wait = response.stated_wait();
        let bytes = response.bytes().await?;
        if (200..300).contains(&status) {
            Ok(bytes)
        } else {
            Err(
                CalDavError::status(status, String::from_utf8_lossy_owned(bytes))
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
        if method == DavMethod::Delete
            && let Some(refused) = refused_before_delete(self, &href, &precondition).await
        {
            return Ok(refused);
        }
        let guarded = !matches!(precondition, Precondition::None);
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
                guarded,
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
#[path = "transport_trust_tests.rs"]
mod transport_trust_tests;

#[cfg(test)]
#[path = "deadline_tests.rs"]
mod deadline_tests;
