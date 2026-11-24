//! [`ManageSieve`]: an account's ManageSieve service, as its providers reach it.
//!
//! Each call opens a session of its own, does its work and logs out: rule edits are rare, and a
//! standing session would hold a slot of the account's budget for nothing. The slot is taken
//! before the dial and held until the session is done (`crate::pool::other`), so the account's
//! IMAP connections and its Sieve session never together go over the ceiling.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use super::{
    dial::{self, SieveEndpoint},
    error::{SieveError, SieveResult},
    session::Session,
};
use crate::{config::ImapConfig, pool::other::Budget};

/// How long one call's session may take, dial, work and `LOGOUT` together: a rule edit is a handful
/// of round trips, each bounded by the engine's reply deadline on its own.
pub(crate) const SESSION_DEADLINE: Duration = Duration::from_mins(2);

/// Opens a signed-in session: the live TLS dial, or a scripted one offline.
pub(crate) type SieveDial<T> =
    Arc<dyn Fn() -> BoxFuture<'static, SieveResult<Session<T>>> + Send + Sync>;

/// An account's ManageSieve service.
pub(crate) struct ManageSieve<T> {
    dial: SieveDial<T>,
    budget: Arc<dyn Budget>,
    /// Whether the last probe of the service succeeded (`crate::managesieve::reach`): only then
    /// are the rules and the reply offered.
    available: AtomicBool,
    /// Whether the last successful probe found `vacation` listed.
    replies: AtomicBool,
    /// Whether the service is to be probed again on the account's next dial: after a failure
    /// that may pass, or `invalidate()`.
    stale: AtomicBool,
}

impl<T> core::fmt::Debug for ManageSieve<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ManageSieve")
            .field("available", &self.available())
            .field("replies", &self.lists_vacation())
            .finish_non_exhaustive()
    }
}

impl<T> ManageSieve<T> {
    /// The service `dial` signs in to, spending from `budget`, offered, with the reply where
    /// `replies`. The account's own service starts unoffered and is offered by its probe
    /// ([`record`](Self::record)).
    pub(crate) fn new(dial: SieveDial<T>, budget: Arc<dyn Budget>, replies: bool) -> Self {
        Self {
            dial,
            budget,
            available: AtomicBool::new(true),
            replies: AtomicBool::new(replies),
            stale: AtomicBool::new(false),
        }
    }

    /// Whether the last probe succeeded.
    pub(crate) fn available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    /// Whether the last successful probe found `vacation` listed.
    pub(crate) fn lists_vacation(&self) -> bool {
        self.replies.load(Ordering::SeqCst)
    }

    /// Records a probe: offered, with the reply where `replies`, or not offered, and whether to
    /// probe again on the next dial.
    pub(crate) fn record(&self, offered: Option<bool>, probe_again: bool) {
        self.available.store(offered.is_some(), Ordering::SeqCst);
        self.replies.store(offered == Some(true), Ordering::SeqCst);
        self.stale.store(probe_again, Ordering::SeqCst);
    }

    /// Asks for a probe on the account's next dial (`ImapAccount::invalidate`).
    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, Ordering::SeqCst);
    }

    /// Whether a probe is wanted, clearing the want: one dial probes, not every one at once.
    pub(crate) fn take_stale(&self) -> bool {
        self.stale.swap(false, Ordering::SeqCst)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ManageSieve<T> {
    /// Runs `work` over a signed-in session, holding a slot of the account's budget throughout,
    /// and sends `LOGOUT` after it whatever it answered, unless the stream failed under it, without
    /// waiting for the answer.
    ///
    /// The dial and the work together get [`SESSION_DEADLINE`]: each wait on the
    /// server is bounded on its own, but a server that answers a byte at a time inside each bound
    /// could hold the slot indefinitely. Past the deadline the session is dropped unannounced, the
    /// slot is given back, and the call fails as a timeout, which is retryable.
    pub(crate) async fn with_session<R, E: From<SieveError>>(
        &self,
        work: impl AsyncFnOnce(&mut Session<T>) -> Result<R, E>,
    ) -> Result<R, E> {
        let _slot = self.budget.reserve().await;
        let wrote = Arc::new(AtomicBool::new(false));
        let session = async {
            let mut session = (self.dial)().await.map_err(E::from)?;
            session.watch_writes(Arc::clone(&wrote));
            let outcome = work(&mut session).await;
            Ok((outcome, session))
        };
        match tokio::time::timeout(SESSION_DEADLINE, session).await {
            Ok(Ok((outcome, session))) => {
                // The work is done and answered: `LOGOUT` goes out, and how slowly the server
                // answers it changes nothing, so nothing waits for it.
                if !session.broken() {
                    session.leave().await;
                }
                outcome
            }
            Ok(Err(err)) => Err(err),
            Err(_) => {
                // Past a write, the store or the switch may have landed: the caller must read
                // before writing again, as after any write whose answer was lost.
                Err(E::from(if wrote.load(Ordering::SeqCst) {
                    SieveError::Unconfirmed
                } else {
                    SieveError::from(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the ManageSieve session did not finish in time",
                    ))
                }))
            }
        }
    }
}

/// The live dial: TLS with the account's connector, a credential from the account's source.
pub(crate) fn dial_tls(
    config: ImapConfig,
    endpoint: SieveEndpoint,
    connector: TlsConnector,
) -> SieveDial<TlsStream<TcpStream>> {
    let shared = Arc::new((config, endpoint, connector));
    Arc::new(move || {
        let shared = Arc::clone(&shared);
        Box::pin(async move {
            let (config, endpoint, connector) = &*shared;
            dial::connect(config, endpoint, connector).await
        })
    })
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
