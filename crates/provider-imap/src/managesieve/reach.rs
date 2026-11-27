//! Whether an account's ManageSieve service answers: asked once the account has connected, and
//! again on a later dial after a failure that may pass or `invalidate()`.
//!
//! The greeting is read, `STARTTLS` run, and what the server offers over TLS read, with no
//! credential: enough to know the endpoint speaks ManageSieve over TLS, at no sign-in a server
//! would count. It runs in a task of its own that nothing waits for, under its own deadline,
//! [`PROBE_DEADLINE`], so an endpoint that never answers delays no connect and no dial. The task
//! belongs to the service and ends when the service is dropped ([`ProbeTask`]).
//!
//! - **The account's first probe, while it runs:** nothing is known yet, so the providers offer the
//!   rules and the reply, and each call waits for the probe before it dials
//!   (`ManageSieve::with_session`). A later probe is waited for by nothing: until it lands, the
//!   last one's finding stands.
//! - **It answered:** the providers offer the rules, and the reply where `vacation` is listed; the
//!   observer sees [`ConnectStep::Negotiated`] with the Sieve extensions.
//! - **It failed, or ran out of time:** neither is offered, and the observer sees
//!   [`ConnectStep::Unavailable`] with the class. Where the class is `Retryable` (the network, a
//!   timeout, a server closing), a call answers `Retryable`
//!   ([`SieveError::NotReached`](super::error::SieveError::NotReached)) and the service is probed
//!   again on the account's next dial once the backoff allows, so a blip at connect does not hide
//!   the rules for the account's lifetime; `invalidate()` asks for that too. A definite answer (no
//!   `STARTTLS`, not a ManageSieve server) makes a call `item_unavailable`
//!   ([`SieveError::NotOffered`](super::error::SieveError::NotOffered)) and is not retried.
//!
//! The probe holds no slot of the account's connection budget: it is one socket, at most one at a
//! time per account, for at most [`PROBE_DEADLINE`], and taking a slot would make it wait behind,
//! or close, the mail's own connections. So while it runs the account may hold one socket more
//! than its ceiling. Each call's session takes a slot (`ManageSieve::with_session`).
//!
//! No credential is presented here, so an offer says the service answers, not that signing in
//! will work: a credential ManageSieve does not carry (`Credential::ActingAs`) or a token where
//! the server offers only `PLAIN` fails at the first call. Sieve failing never fails the mail.

use std::{sync::Arc, time::Duration};

use engine_core::error::FailureClass;
use engine_provider::{ConnectObserver, ConnectService, ConnectStep, ProviderError};
use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    task::JoinHandle,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use super::{
    dial::{self, SieveEndpoint},
    error::SieveResult,
    probe_state::Taken,
    response::Capabilities,
    service::{ManageSieve, dial_tls},
    session::Session,
};
use crate::{config::ImapConfig, pool::other::Budget};

/// How long a probe may take: a healthy service answers its TCP connect, greeting, `STARTTLS` and
/// TLS greeting in well under a second (on the harness, tens of milliseconds); ten seconds leaves
/// a slow one room, and bounds how long a call waits for a probe that is running.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// What a probe reads: the greeting over TLS, or a scripted one offline.
pub(crate) type Greet =
    Arc<dyn Fn() -> BoxFuture<'static, SieveResult<Capabilities>> + Send + Sync>;

/// How an account's service is probed: what reads its greeting, and who is told what it found.
pub(crate) struct Prober {
    greet: Greet,
    observer: Option<Arc<dyn ConnectObserver>>,
}

impl Prober {
    /// Reads the greeting at `endpoint` with the account's connector, telling `config`'s observer.
    pub(crate) fn live(
        config: &ImapConfig,
        endpoint: SieveEndpoint,
        connector: TlsConnector,
    ) -> Self {
        let shared = Arc::new((endpoint, connector));
        Self::new(
            Arc::new(move || {
                let shared = Arc::clone(&shared);
                Box::pin(async move {
                    let (endpoint, connector) = &*shared;
                    greet(dial::open_tls(endpoint, connector).await?).await
                })
            }),
            config.connect_observer.clone(),
        )
    }

    /// Reads the greeting with `greet`, telling `observer`.
    pub(crate) fn new(greet: Greet, observer: Option<Arc<dyn ConnectObserver>>) -> Self {
        Self { greet, observer }
    }

    /// Runs the probe `taken` stands for in a task of its own, under [`PROBE_DEADLINE`], and
    /// records what it found. Nothing waits for the task; dropping the handle ends it.
    fn spawn(&self, taken: Taken) -> ProbeTask {
        let greet = Arc::clone(&self.greet);
        let observer = self.observer.clone();
        ProbeTask(Some(tokio::spawn(async move {
            let probed = tokio::time::timeout(PROBE_DEADLINE, greet())
                .await
                .unwrap_or_else(|_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "the ManageSieve service did not greet in time",
                    )
                    .into())
                });
            let observer = observer
                .as_deref()
                .unwrap_or(&engine_provider::IgnoreConnectSteps);
            land(taken, observer, probed);
        })))
    }
}

/// A probe's task, ended when it is dropped: the service holds it, so no probe outlives the
/// account's providers.
pub(crate) struct ProbeTask(Option<JoinHandle<()>>);

impl ProbeTask {
    /// Lets go of a task that has landed, without ending it.
    fn detach(mut self) {
        self.0.take();
    }
}

impl Drop for ProbeTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

impl<T> ManageSieve<T> {
    /// Starts a probe, unless one runs or nothing probes this service: the account's first.
    pub(crate) fn probe_now(&self) {
        if let Some(prober) = &self.prober
            && let Some(taken) = self.probe.begin()
        {
            let generation = taken.generation();
            self.keep(generation, prober.spawn(taken));
        }
    }

    /// Starts a probe where one is wanted and the backoff allows (`ProbeState::take`), and
    /// answers at once: the dial that asks goes on without it.
    pub(crate) fn reprobe(&self) {
        if let Some(prober) = &self.prober
            && let Some(taken) = self.probe.take()
        {
            let generation = taken.generation();
            self.keep(generation, prober.spawn(taken));
        }
    }

    /// Holds the task of the probe of `generation` until a newer one's or the service's end. A
    /// probe starts only where none runs, so an older one has landed, and its handle is let go
    /// without ending it: two dials can reach here out of order, and the newer probe is the one
    /// the service must be able to end.
    fn keep(&self, generation: u64, task: ProbeTask) {
        let mut held = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held
            .as_ref()
            .is_some_and(|(newest, _)| *newest > generation)
        {
            task.detach();
        } else if let Some((_, older)) = held.replace((generation, task)) {
            older.detach();
        }
    }
}

/// The account's ManageSieve service where `config` names an endpoint, its first probe started.
pub(crate) fn reach(
    config: &ImapConfig,
    connector: &TlsConnector,
    budget: Arc<dyn Budget>,
) -> Option<Arc<ManageSieve<TlsStream<TcpStream>>>> {
    let endpoint = config.managesieve.clone()?;
    let dial = dial_tls(config.clone(), endpoint.clone(), connector.clone());
    let prober = Prober::live(config, endpoint, connector.clone());
    let sieve = Arc::new(ManageSieve::new(dial, budget, false).probed_by(prober));
    // `probed_by` leaves it knowing nothing, a probe wanted, so a first probe that never lands
    // offers nothing.
    sieve.probe_now();
    Some(sieve)
}

/// What a session offers over TLS, read before it leaves without waiting for `LOGOUT`'s answer.
async fn greet<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: Session<T>,
) -> SieveResult<Capabilities> {
    let capabilities = session.capabilities().clone();
    session.leave().await;
    Ok(capabilities)
}

/// Records what the probe found, then tells the observer: offered, with the reply where
/// `vacation` is listed, and the extensions; or the class of the failure, probed again where it
/// may pass. Recorded first, so a host acting on the step finds the accessors already answering.
fn land(taken: Taken, observer: &dyn ConnectObserver, probed: SieveResult<Capabilities>) {
    match probed {
        Ok(capabilities) => {
            taken.record(Some(capabilities.lists("vacation")), false);
            let extensions: Vec<&str> = capabilities.sieve.iter().map(String::as_str).collect();
            observer.step(&ConnectStep::negotiated("ManageSieve", &extensions));
        }
        Err(err) => {
            let class = ProviderError::from(err).class();
            taken.record(None, class == FailureClass::Retryable);
            observer.step(&ConnectStep::unavailable(
                ConnectService::ManageSieve,
                class,
            ));
        }
    }
}

#[cfg(test)]
#[path = "reach_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "reach_account_tests.rs"]
mod account_tests;
