//! Whether an account's ManageSieve service answers: asked when the account connects, and again
//! on a later dial after a failure that may pass or `invalidate()`.
//!
//! The greeting is read, `STARTTLS` run, and what the server offers over TLS read, with no
//! credential: enough to know the endpoint speaks ManageSieve over TLS, at no sign-in a server
//! would count. It runs alongside the account's IMAP connect (`join`), under its own deadline,
//! [`PROBE_DEADLINE`], so an endpoint that never answers costs the connect that and no more, and
//! the answer never depends on which finished first.
//!
//! - **It answered:** the providers offer the rules, and the reply where `vacation` is listed; the
//!   observer sees [`ConnectStep::Negotiated`] with the Sieve extensions.
//! - **It failed, or ran out of time:** neither is offered, and the observer sees
//!   [`ConnectStep::Unavailable`] with the class. Where the class is `Retryable` (the network, a
//!   timeout, a server closing), the service is probed again on the account's next dial, so a blip
//!   at connect does not hide the rules for the account's lifetime; `invalidate()` asks for that
//!   too. A permanent failure (no `STARTTLS`, not a ManageSieve server) is not retried.
//!
//! No credential is presented here, so an offer says the service answers, not that signing in
//! will work: a credential ManageSieve does not carry (`Credential::ActingAs`) or a token where
//! the server offers only `PLAIN` fails at the first call. Sieve failing never fails the mail.

use std::{sync::Arc, time::Duration};

use engine_core::error::FailureClass;
use engine_provider::{ConnectObserver, ConnectService, ConnectStep, ProviderError};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use super::{
    dial::{self, SieveEndpoint},
    error::SieveResult,
    response::Capabilities,
    service::{ManageSieve, dial_tls},
    session::Session,
};
use crate::{config::ImapConfig, pool::other::Budget};

/// How long a probe may take: a healthy service answers its TCP connect, greeting, `STARTTLS` and
/// TLS greeting in well under a second (on the harness, tens of milliseconds); ten seconds leaves
/// a slow one room and bounds what a blackholed endpoint adds to the account's connect.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// Reads the greeting at `endpoint` over TLS and leaves, sending `LOGOUT` without waiting for its
/// answer. It holds no slot of the account's budget: it runs beside a dial, and the two sockets are
/// within any ceiling.
pub(crate) async fn probe(
    endpoint: &SieveEndpoint,
    connector: &TlsConnector,
) -> SieveResult<Capabilities> {
    tokio::time::timeout(PROBE_DEADLINE, async {
        greet(dial::open_tls(endpoint, connector).await?).await
    })
    .await
    .unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the ManageSieve service did not greet in time",
        )
        .into())
    })
}

/// The account's ManageSieve service, recorded with the probe's outcome, where `config` names an
/// endpoint.
pub(crate) fn reach(
    config: &ImapConfig,
    connector: &TlsConnector,
    budget: Arc<dyn Budget>,
    probed: SieveResult<Capabilities>,
) -> Option<Arc<ManageSieve<TlsStream<TcpStream>>>> {
    let endpoint = config.managesieve.clone()?;
    let dial = dial_tls(config.clone(), endpoint, connector.clone());
    let sieve = Arc::new(ManageSieve::new(dial, budget, false));
    record(&sieve, config.connect_observer(), probed);
    Some(sieve)
}

/// Probes `sieve` again where it wants it ([`ManageSieve::take_stale`]), alongside `dial`, and
/// answers what `dial` answered.
pub(crate) async fn reprobe<D: Future>(
    sieve: Option<&ManageSieve<TlsStream<TcpStream>>>,
    config: &ImapConfig,
    connector: &TlsConnector,
    dial: D,
) -> D::Output {
    match (sieve, &config.managesieve) {
        (Some(sieve), Some(endpoint)) if sieve.take_stale() => {
            let (dialled, probed) = tokio::join!(dial, probe(endpoint, connector));
            record(sieve, config.connect_observer(), probed);
            dialled
        }
        _ => dial.await,
    }
}

/// What a session offers over TLS, read before it leaves without waiting for `LOGOUT`'s answer.
async fn greet<T: AsyncRead + AsyncWrite + Unpin + Send>(
    session: Session<T>,
) -> SieveResult<Capabilities> {
    let capabilities = session.capabilities().clone();
    session.leave().await;
    Ok(capabilities)
}

/// Records the probe's outcome on `sieve` and tells the observer: the extensions, or the class of
/// the failure.
fn record<T>(
    sieve: &ManageSieve<T>,
    observer: &dyn ConnectObserver,
    probed: SieveResult<Capabilities>,
) {
    match probed {
        Ok(capabilities) => {
            let extensions: Vec<&str> = capabilities.sieve.iter().map(String::as_str).collect();
            observer.step(&ConnectStep::negotiated("ManageSieve", &extensions));
            sieve.record(Some(capabilities.lists("vacation")), false);
        }
        Err(err) => {
            let class = ProviderError::from(err).class();
            observer.step(&ConnectStep::unavailable(
                ConnectService::ManageSieve,
                class,
            ));
            sieve.record(None, class == FailureClass::Retryable);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{
        error::ImapError,
        managesieve::test_support::{GREETING, scripted},
        mock::MockStream,
    };

    /// How many extensions a greeting's `SIEVE` line names.
    fn capabilities_count(greeting: &[u8]) -> usize {
        let text = String::from_utf8_lossy(greeting);
        let line = text
            .lines()
            .find(|line| line.starts_with("\"SIEVE\""))
            .unwrap();
        line.split('"').nth(3).unwrap().split_whitespace().count()
    }

    /// The service after recording `probed`: offered, with the reply, wanting another probe; and
    /// what the observer saw.
    fn recorded(probed: SieveResult<Capabilities>) -> ((bool, bool, bool), Vec<String>) {
        let (sieve, _) = scripted(&[]);
        let steps = Mutex::new(Vec::new());
        let observer = |step: &ConnectStep<'_>| {
            steps.lock().unwrap().push(match step {
                ConnectStep::Unavailable { service, class, .. } => {
                    format!("unavailable {service:?} {class:?}")
                }
                ConnectStep::Negotiated {
                    dialect, features, ..
                } => format!("{dialect} {}", features.len()),
                _ => "other".to_owned(),
            });
        };
        record(&sieve, &observer, probed);
        let state = (
            sieve.available(),
            sieve.lists_vacation(),
            sieve.take_stale(),
        );
        (state, steps.into_inner().unwrap())
    }

    #[tokio::test(start_paused = true)]
    async fn a_greeting_is_read_with_no_credential_and_left_without_waiting() {
        // The server answers nothing to `LOGOUT`: the probe does not wait for it, not even for a
        // read's stall bound (time is paused, so any wait shows as elapsed time).
        let (stream, recorded_bytes) = MockStream::silent_after(GREETING.to_vec());
        let started = tokio::time::Instant::now();
        let greeting = greet(Session::open(stream).await.unwrap()).await;
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "waited for an answer to LOGOUT"
        );
        assert_eq!(
            crate::mock::written(&recorded_bytes),
            "LOGOUT\r\n",
            "no credential sent"
        );
        let (state, steps) = recorded(greeting);
        assert_eq!(state, (true, true, false), "offered, with the reply");
        assert_eq!(
            steps,
            [format!("ManageSieve {}", capabilities_count(GREETING))]
        );
    }

    #[test]
    fn a_failure_offers_nothing_and_one_that_may_pass_is_probed_again() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x.example");
        let (state, steps) = recorded(Err(ImapError::Io(refused).into()));
        assert_eq!(state, (false, false, true));
        assert_eq!(steps, ["unavailable ManageSieve Retryable"]);
        // No STARTTLS, or no ManageSieve at all: not offered, and not probed again.
        let (state, steps) = recorded(Err(ImapError::protocol("no STARTTLS").into()));
        assert_eq!(state, (false, false, false));
        assert_eq!(steps, ["unavailable ManageSieve Permanent"]);
        // Offered without the reply where `vacation` is not listed.
        let (state, _) = recorded(Ok(Capabilities::default()));
        assert_eq!(state, (true, false, false));
    }

    #[tokio::test]
    async fn a_dial_probes_again_only_when_the_service_wants_it() {
        let dial: super::super::service::SieveDial<TlsStream<TcpStream>> =
            Arc::new(|| unreachable!("a probe signs in to nothing"));
        let budget = Arc::new(crate::managesieve::test_support::OneSlot(Arc::new(
            tokio::sync::Semaphore::new(1),
        )));
        let sieve = ManageSieve::new(dial, budget, false);
        let steps = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&steps);
        // Nothing listens on port 1: a probe there fails at once, as one that may pass.
        let config = ImapConfig::new(
            "h:993",
            "h",
            engine_provider::Credential::password("a", "b"),
        )
        .with_managesieve("127.0.0.1:1", "localhost")
        .with_connect_observer(Arc::new(move |_: &ConnectStep<'_>| {
            *seen.lock().unwrap() += 1;
        }));
        let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
        assert_eq!(
            reprobe(Some(&sieve), &config, &connector, async { 7 }).await,
            7
        );
        assert_eq!(*steps.lock().unwrap(), 0, "not wanted: no probe");
        sieve.mark_stale();
        assert_eq!(
            reprobe(Some(&sieve), &config, &connector, async { 8 }).await,
            8
        );
        assert_eq!(
            *steps.lock().unwrap(),
            1,
            "wanted: one probe, beside the dial"
        );
        assert!(!sieve.available());
        assert!(
            sieve.take_stale(),
            "it failed as one that may pass: wanted again"
        );
    }

    #[tokio::test]
    async fn the_accessors_answer_only_what_the_last_probe_found() {
        use engine_core::ids::MailboxId;
        use engine_provider::Provider;

        let (stream, _) = MockStream::new(b"* OK ready\r\n".to_vec());
        let connection = crate::transport::Connection::open(stream).await.unwrap();
        let (sieve, _) = scripted(&[]);
        let sieve = Arc::new(sieve);
        let provider =
            crate::ImapProvider::with_connection(connection, MailboxId::try_from("INBOX").unwrap())
                .with_sieve(Some(Arc::clone(&sieve) as Arc<dyn super::super::Sieve>));
        let offered = |provider: &crate::ImapProvider<MockStream>| {
            (
                provider.mail_rules().is_some(),
                provider.vacation().is_some(),
            )
        };
        sieve.record(Some(false), false);
        assert_eq!(
            offered(&provider),
            (true, false),
            "no vacation listed: no reply"
        );
        sieve.record(None, true);
        assert_eq!(offered(&provider), (false, false), "the last probe failed");
        sieve.record(Some(true), false);
        assert_eq!(offered(&provider), (true, true));
    }
}
