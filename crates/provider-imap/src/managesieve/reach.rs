//! Whether an account's ManageSieve service answers, asked when the account connects.
//!
//! The greeting is read, `STARTTLS` run, and what the server offers over TLS read, with no
//! credential: enough to know the endpoint speaks ManageSieve over TLS, at no sign-in a server
//! would count. It runs beside the account's first IMAP connection and never delays it: where it
//! has not finished when the mail has connected, it is dropped.
//!
//! - **It answered:** the providers offer the rules, and the reply where `vacation` is listed; the
//!   observer sees [`ConnectStep::Negotiated`] with the Sieve extensions.
//! - **It failed in a way that may pass** (a `Retryable` class: the network, a timeout, a server
//!   closing), or had not finished: the providers offer both all the same, and each call, which
//!   opens a session of its own, fails or succeeds on its own; the observer sees
//!   [`ConnectStep::Unavailable`] with that class. A blip at connect does not hide the rules for
//!   the account's lifetime.
//! - **It failed in a way that will not pass** (no `STARTTLS`, not a ManageSieve server, a bad TLS
//!   name): neither is offered, and the observer sees [`ConnectStep::Unavailable`] with the class.
//!
//! No credential is presented here, so `Some` says the service answers, not that sign-in will
//! work: a credential ManageSieve cannot carry (`Credential::ActingAs`) or a token against a
//! server offering only `PLAIN` fails at the first call. Sieve failing never fails the mail.

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_provider::{ConnectObserver, ConnectService, ConnectStep, ProviderError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

use super::{
    Sieve,
    dial::{self, SieveEndpoint},
    error::SieveResult,
    response::Capabilities,
    service::{ManageSieve, dial_tls},
    session::Session,
};
use crate::{config::ImapConfig, pool::other::Budget};

/// Reads the greeting at `endpoint` over TLS, and logs out. It holds no slot of the account's
/// budget: it runs beside the account's first connection, before the account's pool exists, and
/// the two sockets are within any ceiling.
pub(crate) async fn probe(
    endpoint: &SieveEndpoint,
    connector: &TlsConnector,
) -> SieveResult<Capabilities> {
    greet(dial::open_tls(endpoint, connector).await?).await
}

/// The account's rules and reply after the probe answered `probed`, or `None` where it had not
/// finished ([module docs](self)). `None` where `config` names no endpoint.
pub(crate) fn reach(
    config: &ImapConfig,
    connector: &TlsConnector,
    budget: Arc<dyn Budget>,
    probed: Option<SieveResult<Capabilities>>,
) -> Option<Arc<dyn Sieve>> {
    let endpoint = config.managesieve.clone()?;
    let replies = reported(config.connect_observer(), probed)?;
    let dial = dial_tls(config.clone(), endpoint, connector.clone());
    Some(Arc::new(ManageSieve::new(dial, budget, replies)))
}

/// What a session offers over TLS, read before it logs out.
async fn greet<T: AsyncRead + AsyncWrite + Unpin + Send>(
    mut session: Session<T>,
) -> SieveResult<Capabilities> {
    let capabilities = session.capabilities().clone();
    session.logout().await;
    Ok(capabilities)
}

/// The probe's outcome, told to the observer, and what it means for the account: `Some(replies)`
/// where the service is offered (`replies`: whether the reply is offered too), `None` where it is
/// not.
fn reported(
    observer: &dyn ConnectObserver,
    probed: Option<SieveResult<Capabilities>>,
) -> Option<bool> {
    let failed = |class: FailureClass| {
        observer.step(&ConnectStep::unavailable(
            ConnectService::ManageSieve,
            class,
        ));
        // Only a failure that may pass leaves the service offered; its calls find out.
        (class == FailureClass::Retryable).then_some(true)
    };
    match probed {
        Some(Ok(capabilities)) => {
            let extensions: Vec<&str> = capabilities.sieve.iter().map(String::as_str).collect();
            observer.step(&ConnectStep::negotiated("ManageSieve", &extensions));
            Some(capabilities.lists("vacation"))
        }
        Some(Err(err)) => failed(ProviderError::from(err).class()),
        // Not finished when the mail had connected: not yet known to fail.
        None => failed(FailureClass::Retryable),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{error::ImapError, managesieve::test_support::GREETING, mock::MockStream};

    /// How many extensions a greeting's `SIEVE` line names.
    fn capabilities_count(greeting: &[u8]) -> usize {
        let text = String::from_utf8_lossy(greeting);
        let line = text
            .lines()
            .find(|line| line.starts_with("\"SIEVE\""))
            .unwrap();
        line.split('"').nth(3).unwrap().split_whitespace().count()
    }

    fn seen(probed: Option<SieveResult<Capabilities>>) -> (Option<bool>, Vec<String>) {
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
        let kept = reported(&observer, probed);
        (kept, steps.into_inner().unwrap())
    }

    #[tokio::test]
    async fn a_greeting_is_read_with_no_credential_and_reported_with_its_extensions() {
        let mut script = GREETING.to_vec();
        script.extend_from_slice(b"OK\r\n");
        let (stream, recorded) = MockStream::new(script);
        let greeting = greet(Session::open(stream).await.unwrap()).await;
        assert_eq!(
            crate::mock::written(&recorded),
            "LOGOUT\r\n",
            "no credential sent"
        );
        let (kept, steps) = seen(Some(greeting));
        assert_eq!(
            kept,
            Some(true),
            "the reply is offered: `vacation` is listed"
        );
        assert_eq!(
            steps,
            [format!("ManageSieve {}", capabilities_count(GREETING))]
        );
    }

    #[test]
    fn a_failure_that_may_pass_leaves_the_service_offered_and_one_that_will_not_does_not() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x.example");
        let (kept, steps) = seen(Some(Err(ImapError::Io(refused).into())));
        assert_eq!(kept, Some(true));
        assert_eq!(steps, ["unavailable ManageSieve Retryable"]);
        // Not finished when the mail had connected: offered, and said so.
        let (kept, steps) = seen(None);
        assert_eq!(kept, Some(true));
        assert_eq!(steps, ["unavailable ManageSieve Retryable"]);
        // No STARTTLS, or no ManageSieve at all: not offered.
        let (kept, steps) = seen(Some(Err(ImapError::protocol("no STARTTLS").into())));
        assert_eq!(kept, None);
        assert_eq!(steps, ["unavailable ManageSieve Permanent"]);
    }
}
