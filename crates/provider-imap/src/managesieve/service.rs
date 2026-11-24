//! [`ManageSieve`]: an account's ManageSieve service, as its providers reach it.
//!
//! Each call opens a session of its own, does its work and logs out: rule edits are rare, and a
//! standing session would hold a slot of the account's budget for nothing. The slot is taken
//! before the dial and held until the session is done (`crate::pool::other`), so the account's
//! IMAP connections and its Sieve session never together go over the ceiling.

use std::{sync::Arc, time::Duration};

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
    /// Whether the server listed `vacation` when the account connected: where it did not, the
    /// account has no out-of-office reply to offer.
    replies: bool,
}

impl<T> core::fmt::Debug for ManageSieve<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ManageSieve")
            .field("replies", &self.replies)
            .finish_non_exhaustive()
    }
}

impl<T> ManageSieve<T> {
    /// The service `dial` signs in to, spending from `budget`; `replies`: whether to offer the
    /// out-of-office reply (the server listed `vacation`, or is not yet known not to).
    pub(crate) fn new(dial: SieveDial<T>, budget: Arc<dyn Budget>, replies: bool) -> Self {
        Self {
            dial,
            budget,
            replies,
        }
    }

    /// Whether the out-of-office reply is offered.
    pub(crate) fn lists_vacation(&self) -> bool {
        self.replies
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ManageSieve<T> {
    /// Runs `work` over a signed-in session, holding a slot of the account's budget throughout,
    /// and logs out after it whatever it answered, unless the stream failed under it.
    ///
    /// The dial, the work and the `LOGOUT` together get [`SESSION_DEADLINE`]: each wait on the
    /// server is bounded on its own, but a server that answers a byte at a time inside each bound
    /// could hold the slot indefinitely. Past the deadline the session is dropped unannounced, the
    /// slot is given back, and the call fails as a timeout, which is retryable.
    pub(crate) async fn with_session<R, E: From<SieveError>>(
        &self,
        work: impl AsyncFnOnce(&mut Session<T>) -> Result<R, E>,
    ) -> Result<R, E> {
        let _slot = self.budget.reserve().await;
        let session = async {
            let mut session = (self.dial)().await.map_err(E::from)?;
            let outcome = work(&mut session).await;
            if !session.broken() {
                session.logout().await;
            }
            outcome
        };
        tokio::time::timeout(SESSION_DEADLINE, session)
            .await
            .unwrap_or_else(|_| {
                Err(E::from(SieveError::from(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the ManageSieve session did not finish in time",
                ))))
            })
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
