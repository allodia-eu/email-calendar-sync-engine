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
    probe_state::ProbeState,
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
    /// What the last probe found, and whether a dial is to probe again
    /// (`crate::managesieve::reach`): the rules and the reply are offered only as it says.
    pub(crate) probe: ProbeState,
}

impl<T> core::fmt::Debug for ManageSieve<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ManageSieve")
            .field("offered", &self.probe.offered())
            .finish_non_exhaustive()
    }
}

impl<T> ManageSieve<T> {
    /// The service `dial` signs in to, spending from `budget`, offered, with the reply where
    /// `replies`. The account's own service is then recorded with its probe
    /// ([`ProbeState::record`]).
    pub(crate) fn new(dial: SieveDial<T>, budget: Arc<dyn Budget>, replies: bool) -> Self {
        Self {
            dial,
            budget,
            probe: ProbeState::offered_with(replies),
        }
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
