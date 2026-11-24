//! [`AuthenticatedClient`]: the one way an HTTP request that carries an account's credential
//! goes out (`docs/design/credential-source.md`).
//!
//! The adapter holds the account's [`CredentialSource`], never the credential, and the client
//! asks it each time a request is about to go out, so a token the host refreshed is the one
//! presented and no adapter has to be rebuilt for it. The client is built here, for the account's
//! configured origin, and every request goes out on it whatever client built the request, so
//! the rules below hold for every adapter without one having to remember them:
//!
//! - **No redirect is followed.** The origin is checked on each request's own URL; a followed
//!   redirect would carry the credential past every rule here. An adapter follows a redirect, where
//!   its protocol has one, as a request of its own.
//! - **No proxy sees a plaintext credential.** An account configured on plaintext loopback gets a
//!   client that ignores the environment's proxies (built with `deadline::client_for`); every other
//!   account keeps them.
//! - **No credential over plaintext HTTP, except to loopback from an account that lives there.** An
//!   `http://` origin receives nothing, and the request fails as `Permanent`, unless it is the name
//!   `localhost` or a loopback address (`127.0.0.0/8`, `::1`) **and** the account's configured origin
//!   is itself plaintext loopback (a test harness, a server on the same machine). An account configured
//!   over `https` never sends its credential over `http://`, loopback included: a session or a redirect
//!   naming `http://127.0.0.1:…` would otherwise hand it, in cleartext, to whatever listens on the
//!   worker's own loopback, which a multi-tenant host shares. It is an engine rule, not a default
//!   of [`permits`](CredentialSource::permits), and a host can still refuse loopback plaintext
//!   through its source.
//! - **The source must [permit](CredentialSource::permits) the origin**, before the first byte goes
//!   to it.
//! - **An ask has the request's deadline**, [`Deadlines::reply`]: nothing has been sent yet, so a
//!   source that does not answer in time fails the request as `Retryable`, a submission's included.
//! - **One replacement and one replay, only on a `401`.** Only a `401` response to a request that
//!   carried the credential is judged, by the adapter's reading of its challenge
//!   ([`Unauthorized`]). A `403` is never a refusal, and neither is a failed exchange: the request
//!   may have been received, and a `401` says only that it was not processed, which is what makes
//!   replaying a submission safe. A body that cannot be cloned is not replayed, and the `401` comes
//!   back as it came.

use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use engine_provider::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource,
    Deadlines, Origin, with_credential,
};
use reqwest::{Client, Method, Request, RequestBuilder, StatusCode, Url, redirect::Policy};

use crate::{Exchange, RetryConfig, SendError, Sent, send_retrying};

/// How much of a refused reply's body is read before the source is asked for a replacement.
/// The body is a diagnostic, nothing depends on it, and a refusal is no place for a large one.
const REFUSAL_BODY: usize = 64 * 1024;

/// How long that read may take, all of it: a trickling refusal must not hold the request.
const REFUSAL_READ: Duration = Duration::from_secs(5);

/// What a `401` to a request that carried the credential says, as the adapter reads the
/// server's challenge (`WWW-Authenticate`, RFC 9110 §11.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unauthorized {
    /// The server judged the credential and refused it: its challenge offers the scheme the
    /// credential was presented under, or it sent none. The source is asked for a
    /// replacement, once, and the request is sent again with it.
    Refused,
    /// The server refused how the credential was framed, not the credential: JMAP's challenge
    /// naming Bearer to a password presented as Basic. The same credential is presented again,
    /// framed as the adapter has now chosen, once per credential. Never also a replacement for
    /// the same `401`.
    Reframe,
    /// Neither another framing nor another credential of this kind can answer it (the
    /// challenge offers only schemes the adapter cannot present). The `401` comes back as it
    /// came.
    Final,
}

/// An account's HTTP client for requests that carry its credential: the account's configured
/// origin, its [`CredentialSource`], its [`RetryConfig`], and a `reqwest` client built for them
/// that follows no redirect.
///
/// Build requests with [`request`](Self::request) (or [`get`](Self::get),
/// [`post`](Self::post)) and send them with [`send`](Self::send). A request built by any other
/// client goes out on this one all the same, so its redirect and proxy settings never apply.
pub struct AuthenticatedClient {
    client: Client,
    configured: Url,
    source: Arc<dyn CredentialSource>,
    retry: RetryConfig,
}

/// Names the configured origin, which is the host's own configuration; never the credential.
impl core::fmt::Debug for AuthenticatedClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthenticatedClient")
            // The origin only: a host may have put a password or a token in the URL.
            .field(
                "configured",
                &self.configured.origin().ascii_serialization(),
            )
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl AuthenticatedClient {
    /// The client for the account configured at `configured` (the URL the host gave the
    /// adapter), trusting per `tls`, naming `user_agent`, asking `source`, and waiting out
    /// throttles as `retry` says.
    ///
    /// # Errors
    ///
    /// [`SendError`] when the HTTP client cannot be built (a TLS backend that cannot start).
    pub fn new(
        tls: &engine_tls::TlsClientConfig,
        user_agent: &crate::UserAgent,
        configured: Url,
        source: Arc<dyn CredentialSource>,
        retry: RetryConfig,
    ) -> Result<Self, SendError> {
        let client = crate::deadline::client_for(tls, user_agent, &configured)
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client,
            configured,
            source,
            retry,
        })
    }

    /// A request to `url`, to fill in and [`send`](Self::send).
    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.client.request(method, url)
    }

    /// A `GET` of `url`.
    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    /// A `POST` to `url`.
    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request(Method::POST, url)
    }

    /// The origin the host configured the account at.
    #[must_use]
    pub fn configured(&self) -> &Url {
        &self.configured
    }

    /// Whether a request to `url` may carry the credential at all: the plaintext rule
    /// [`send`](Self::send) applies first, then the source's
    /// [`permits`](CredentialSource::permits). Asks the source for nothing.
    ///
    /// For an adapter that decides before it sends whether an origin may ever receive the
    /// credential, such as a discovery that refuses to move to an origin the credential could not
    /// follow it to. [`send`](Self::send) checks the same again for every request, so an adapter
    /// that skips this sends nothing it should not.
    #[must_use]
    pub fn admits(&self, url: &Url) -> bool {
        credential_origin(url, &self.configured).is_ok_and(|origin| self.source.permits(&origin))
    }

    /// Sends `request` on this client with a credential from the source, presented by
    /// `present`, and once more with a replacement if the server refuses it.
    ///
    /// 1. The request's origin must be `https`, or `http` to loopback from an account configured on
    ///    plaintext loopback too, and the source must permit it. Otherwise nothing is sent, and the
    ///    result is a `Permanent` [`CredentialFailure`].
    /// 2. The source is asked for a credential under [`Deadlines::reply`]; a source that does not
    ///    answer in time fails as `Retryable`.
    /// 3. `present` adds the credential to the request, and it goes through [`send_retrying`], so a
    ///    throttle is waited out and a silent server bounded exactly as an anonymous request is. An
    ///    error from `present` (a kind of credential the adapter cannot send) ends the request
    ///    before anything is sent.
    /// 4. A `401` is handed to `judge` with the credential it refused. On
    ///    [`Reframe`](Unauthorized::Reframe) the same credential is presented again, once. On
    ///    [`Refused`](Unauthorized::Refused) the source is asked to
    ///    [replace](CredentialSource::replace) it, under the same deadline, and the request goes
    ///    once more with the replacement. A second refusal, a source with no replacement, and a
    ///    request whose body cannot be cloned all hand the `401` back, which the adapter reads as
    ///    the authentication failure it is.
    ///
    /// Before `replace` is asked, up to 64 KiB of the refused reply's body is read, for no
    /// longer than five seconds, so its connection is free while the source refreshes. That read
    /// is best effort: a body cut off, too large or trickling leaves the reply a refusal as it
    /// stands, never a failed exchange, because a `401` says the request was not processed (a
    /// submission's included). A source that fails or does not answer when asked to replace ends
    /// the request with its own failure, in place of the `401`: a request was sent then, and
    /// refused.
    ///
    /// No other reply is judged and nothing else is replayed: a `403` is the server's answer
    /// about the request, and a failed exchange may have been received.
    ///
    /// # Errors
    ///
    /// `E` from a [`SendError`] (the exchange failed, or the request could not be built), from a
    /// [`CredentialFailure`] (the origin was refused, or the source failed, with its class,
    /// whether asked before the first request or for a replacement after a `401`), or from
    /// `present`.
    ///
    /// # Panics
    ///
    /// Outside a tokio runtime with the timer enabled, as [`send_retrying`] does.
    pub async fn send<E>(
        &self,
        request: RequestBuilder,
        exchange: Exchange<'_>,
        present: impl Fn(RequestBuilder, &Credential) -> Result<RequestBuilder, E>,
        judge: impl Fn(&Sent, &Credential) -> Unauthorized,
    ) -> Result<Sent, E>
    where
        E: From<SendError> + From<CredentialFailure>,
    {
        // Only the request is kept: it goes out on this client, whichever built it.
        let (_, built) = request.build_split();
        let request = built.map_err(SendError::from)?;
        let (client, source, configured, retry) = (
            &self.client,
            self.source.as_ref(),
            &self.configured,
            &self.retry,
        );
        let origin = credential_origin(request.url(), configured)?;
        let ask = CredentialRequest::new(CredentialProtocol::Http, &origin);
        let presentation = Presentation {
            client,
            stash: Mutex::new(Some(request)),
            retry,
            exchange,
            present: &present,
            judge: &judge,
        };
        let outcome = with_credential(
            source,
            &ask,
            Deadlines::STANDARD.reply(),
            |attempt: &Attempt<E>| {
                matches!(
                    attempt,
                    Attempt::Refused {
                        replayable: true,
                        ..
                    }
                )
            },
            |credential| presentation.attempt(credential),
        )
        .await;
        match outcome {
            Ok(sent) => Ok(sent),
            Err(Attempt::Refused { sent, .. }) => Ok(*sent),
            Err(Attempt::Failed(err)) => Err(err),
        }
    }
}

/// Where a request carrying a credential is going, once the plaintext rule has allowed it.
///
/// reqwest builds a request only for an `http` or `https` URL, and those always name a host
/// and have a known default port, so the fallbacks below never apply.
fn credential_origin(url: &Url, configured: &Url) -> Result<Origin, CredentialFailure> {
    match url.scheme() {
        "https" => {}
        "http" if plaintext_loopback(url) && plaintext_loopback(configured) => {}
        _ => {
            return Err(CredentialFailure::refused_origin(
                "a credential goes over plaintext HTTP only to loopback, from an account \
                 configured there",
            ));
        }
    }
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or_default();
    Ok(Origin::new(url.scheme(), host, port))
}

/// Whether `url` is plaintext HTTP to a loopback host.
pub(crate) fn plaintext_loopback(url: &Url) -> bool {
    url.scheme() == "http" && is_loopback(url.host_str().unwrap_or_default())
}

/// Whether `host`, as a URL serialises it, is the name `localhost` or a loopback address:
/// `127.0.0.0/8`, or `::1` (bracketed in a URL).
fn is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6.parse::<Ipv6Addr>().is_ok_and(|ip| ip.is_loopback());
    }
    host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// How one presentation of a credential ended, as [`with_credential`] sees it.
enum Attempt<E> {
    /// The server refused the credential; `replayable` when the request can go again.
    Refused { sent: Box<Sent>, replayable: bool },
    /// Anything else that failed: the exchange, `present`, or the source.
    Failed(E),
}

impl<E: From<CredentialFailure>> From<CredentialFailure> for Attempt<E> {
    fn from(failure: CredentialFailure) -> Self {
        Self::Failed(failure.into())
    }
}

/// One request and what presents a credential on it, shared by the first presentation and
/// a replacement's.
struct Presentation<'a, P, J> {
    client: &'a Client,
    /// The request the next presentation sends. Each takes it and leaves a clone behind, so it
    /// is empty once a body could not be cloned, and nothing is replayed after that.
    stash: Mutex<Option<Request>>,
    retry: &'a RetryConfig,
    exchange: Exchange<'a>,
    present: &'a P,
    judge: &'a J,
}

impl<P, J> Presentation<'_, P, J> {
    /// Sends the request with `credential`, and again with it re-framed if the adapter says
    /// the framing, not the credential, was refused.
    async fn attempt<E>(&self, credential: Credential) -> Result<Sent, Attempt<E>>
    where
        E: From<SendError>,
        P: Fn(RequestBuilder, &Credential) -> Result<RequestBuilder, E>,
        J: Fn(&Sent, &Credential) -> Unauthorized,
    {
        let mut reframed = false;
        loop {
            // Never empty here: the first presentation finds the request, and every later one
            // runs only after `replayable` saw a clone left behind.
            let request = self
                .take()
                .expect("a request is presented again only when a clone was kept");
            let builder = RequestBuilder::from_parts(self.client.clone(), request);
            let builder = (self.present)(builder, &credential).map_err(Attempt::Failed)?;
            // Boxed: the exchange is most of this future's size, and the ask, the replacement and
            // the re-framing around it would otherwise make every caller's future that much
            // larger again. One allocation per presentation is nothing beside a request.
            let sent = Box::pin(send_retrying(builder, self.retry, self.exchange))
                .await
                .map_err(|err| Attempt::Failed(err.into()))?;
            if sent.status() != StatusCode::UNAUTHORIZED {
                return Ok(sent);
            }
            let replayable = self.replayable();
            match (self.judge)(&sent, &credential) {
                Unauthorized::Reframe if replayable && !reframed => reframed = true,
                Unauthorized::Refused => {
                    // Read now, so the connection is free while the source refreshes; the
                    // reply comes back as it came if there is no replacement. Best effort: a
                    // body that breaks off leaves a refusal, never a failed exchange.
                    let sent = if replayable {
                        sent.read_refusal(REFUSAL_BODY, REFUSAL_READ).await
                    } else {
                        sent
                    };
                    let sent = Box::new(sent);
                    return Err(Attempt::Refused { sent, replayable });
                }
                Unauthorized::Reframe | Unauthorized::Final => return Ok(sent),
            }
        }
    }

    /// The request to send now, leaving a clone for the next presentation where its body can
    /// be cloned.
    fn take(&self) -> Option<Request> {
        let mut stash = self.stash.lock().unwrap_or_else(PoisonError::into_inner);
        let request = stash.take()?;
        *stash = request.try_clone();
        Some(request)
    }

    /// Whether the request can be sent again.
    fn replayable(&self) -> bool {
        self.stash
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

#[cfg(test)]
#[path = "authenticated_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "authenticated_reply_tests.rs"]
mod reply_tests;

#[cfg(test)]
#[path = "authenticated_wire_tests.rs"]
mod wire_tests;

#[cfg(test)]
#[path = "authenticated_admits_tests.rs"]
mod admits_tests;
