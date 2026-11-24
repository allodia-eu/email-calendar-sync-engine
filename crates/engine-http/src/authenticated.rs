//! [`send_authenticated`]: the one funnel every HTTP request that carries an account's
//! credential goes through (`docs/design/credential-source.md`).
//!
//! The adapter holds the account's [`CredentialSource`], never the credential, and this asks
//! it each time a request is about to go out, so a token the host refreshed is the one
//! presented and no adapter has to be rebuilt for it. On the way it applies the rules that no
//! source can loosen and no adapter can forget, because there is no other way to send:
//!
//! - **No credential over plaintext HTTP, except to loopback.** An `http://` origin other than the
//!   name `localhost` or a loopback address (`127.0.0.0/8`, `::1`) receives nothing, and the
//!   request fails as `Permanent`. It is an engine rule, not a default of
//!   [`permits`](CredentialSource::permits): every live harness serves on `127.0.0.1`, and a host
//!   can still refuse loopback plaintext through its source.
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
    sync::{Mutex, PoisonError},
};

use engine_provider::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource,
    Deadlines, Origin, with_credential,
};
use reqwest::{Client, Request, RequestBuilder, StatusCode, Url};

use crate::{Exchange, RetryConfig, SendError, Sent, send_retrying};

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

/// Sends `request` with a credential from `source`, presented by `present`, and once more with
/// a replacement if the server refuses it.
///
/// 1. The request's origin must be `https`, or `http` to loopback, and the source must permit it.
///    Otherwise nothing is sent, and the result is a `Permanent` [`CredentialFailure`].
/// 2. The source is asked for a credential under [`Deadlines::reply`]; a source that does not
///    answer in time fails as `Retryable`.
/// 3. `present` adds the credential to the request, and it goes through [`send_retrying`], so a
///    throttle is waited out and a silent server bounded exactly as an anonymous request is. An
///    error from `present` (a kind of credential the adapter cannot send) ends the request before
///    anything is sent.
/// 4. A `401` is handed to `judge` with the credential it refused. On
///    [`Reframe`](Unauthorized::Reframe) the same credential is presented again, once. On
///    [`Refused`](Unauthorized::Refused) the source is asked to
///    [replace](CredentialSource::replace) it, under the same deadline, and the request goes once
///    more with the replacement. A second refusal, a source with no replacement, and a request
///    whose body cannot be cloned all hand the `401` back, which the adapter reads as the
///    authentication failure it is.
///
/// No other reply is judged and nothing else is replayed: a `403` is the server's answer
/// about the request, and a failed exchange may have been received.
///
/// # Errors
///
/// `E` from a [`SendError`] (the exchange failed, or the request could not be built), from a
/// [`CredentialFailure`] (the origin was refused, or the source failed, with its class), or
/// from `present`.
///
/// # Panics
///
/// Outside a tokio runtime with the timer enabled, as [`send_retrying`] does.
pub async fn send_authenticated<E>(
    source: &dyn CredentialSource,
    request: RequestBuilder,
    retry: &RetryConfig,
    exchange: Exchange<'_>,
    present: impl Fn(RequestBuilder, &Credential) -> Result<RequestBuilder, E>,
    judge: impl Fn(&Sent, &Credential) -> Unauthorized,
) -> Result<Sent, E>
where
    E: From<SendError> + From<CredentialFailure>,
{
    let (client, built) = request.build_split();
    let request = built.map_err(SendError::from)?;
    let origin = credential_origin(request.url())?;
    let ask = CredentialRequest::new(CredentialProtocol::Http, &origin);
    let presentation = Presentation {
        client: &client,
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

/// Where a request carrying a credential is going, once the plaintext rule has allowed it.
fn credential_origin(url: &Url) -> Result<Origin, CredentialFailure> {
    let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(CredentialFailure::refused_origin(
            "a request without a host cannot carry a credential",
        ));
    };
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(host) => {}
        _ => {
            return Err(CredentialFailure::refused_origin(
                "a credential goes over plaintext HTTP only to a loopback origin",
            ));
        }
    }
    Ok(Origin::new(url.scheme(), host, port))
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
            let Some(request) = self.take() else {
                unreachable!("a request is presented again only when a clone was kept")
            };
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
