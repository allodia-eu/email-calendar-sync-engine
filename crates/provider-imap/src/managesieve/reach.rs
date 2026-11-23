//! Whether an account's ManageSieve service answers, asked once when the account connects.
//!
//! The greeting is read, `STARTTLS` run, and what the server offers over TLS read, with no
//! credential: that is enough to know the endpoint is a ManageSieve server the engine can sign in
//! to over TLS, and costs no sign-in a server would count. Where it answers, the account's
//! providers offer the rules and the reply, and the observer sees
//! [`ConnectStep::Negotiated`] with the Sieve extensions listed; where it does not, the account's
//! mail works without them, and the observer sees [`ConnectStep::Unavailable`] with the class of
//! the failure and nothing else. Sieve failing never fails the account's mail.

use std::{future::Future, sync::Arc};

use engine_provider::{ConnectObserver, ConnectService, ConnectStep, ProviderError};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

use super::{
    Sieve, dial,
    error::SieveResult,
    response::Capabilities,
    service::{ManageSieve, dial_tls},
    session::Session,
};
use crate::{config::ImapConfig, pool::other::Budget};

/// The account's rules and reply, where `config` names a ManageSieve endpoint and it answers.
pub(crate) async fn reach(
    config: &ImapConfig,
    connector: &TlsConnector,
    budget: Arc<dyn Budget>,
) -> Option<Arc<dyn Sieve>> {
    let endpoint = config.managesieve.clone()?;
    let greeting = greet(budget.as_ref(), dial::open_tls(&endpoint, connector)).await;
    let capabilities = reported(config.connect_observer(), greeting)?;
    let dial = dial_tls(config.clone(), endpoint, connector.clone());
    Some(Arc::new(ManageSieve::new(dial, budget, &capabilities)))
}

/// What the server offers over TLS, read in a slot of the account's budget by a session that
/// then logs out.
async fn greet<T: AsyncRead + AsyncWrite + Unpin + Send>(
    budget: &dyn Budget,
    open: impl Future<Output = SieveResult<Session<T>>>,
) -> SieveResult<Capabilities> {
    let _slot = budget.reserve().await;
    let session = open.await?;
    let capabilities = session.capabilities().clone();
    session.logout().await;
    Ok(capabilities)
}

/// The greeting's outcome, told to the observer: the extensions, or the class of the failure.
fn reported(
    observer: &dyn ConnectObserver,
    greeting: SieveResult<Capabilities>,
) -> Option<Capabilities> {
    match greeting {
        Ok(capabilities) => {
            let extensions: Vec<&str> = capabilities.sieve.iter().map(String::as_str).collect();
            observer.step(&ConnectStep::negotiated("ManageSieve", &extensions));
            Some(capabilities)
        }
        Err(err) => {
            let class = ProviderError::from(err).class();
            observer.step(&ConnectStep::unavailable(
                ConnectService::ManageSieve,
                class,
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use engine_core::error::FailureClass;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::{
        error::ImapError,
        managesieve::test_support::{GREETING, OneSlot},
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

    fn seen(greeting: SieveResult<Capabilities>) -> (Option<Capabilities>, Vec<String>) {
        let steps = Mutex::new(Vec::new());
        let observer = |step: &ConnectStep<'_>| {
            steps.lock().unwrap().push(match step {
                ConnectStep::Unavailable { service, class, .. } => {
                    format!("unavailable {service:?} {class:?}")
                }
                ConnectStep::Negotiated {
                    dialect, features, ..
                } => {
                    format!("{dialect} {}", features.len())
                }
                _ => "other".to_owned(),
            });
        };
        let kept = reported(&observer, greeting);
        (kept, steps.into_inner().unwrap())
    }

    #[tokio::test]
    async fn a_greeting_is_read_in_a_slot_and_reported_with_its_extensions() {
        let budget = OneSlot(Arc::new(Semaphore::new(1)));
        let mut script = GREETING.to_vec();
        script.extend_from_slice(b"OK\r\n");
        let (stream, recorded) = MockStream::new(script);
        let greeting = greet(&budget, Session::open(stream)).await;
        assert_eq!(
            crate::mock::written(&recorded),
            "LOGOUT\r\n",
            "no credential sent"
        );
        assert_eq!(budget.0.available_permits(), 1, "the slot is given back");
        let (kept, steps) = seen(greeting);
        assert!(kept.is_some_and(|capabilities| capabilities.lists("vacation")));
        let listed = capabilities_count(GREETING);
        assert_eq!(steps, [format!("ManageSieve {listed}")]);
    }

    #[tokio::test]
    async fn a_service_that_does_not_answer_is_reported_by_class_alone() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "secret.example");
        let budget = OneSlot(Arc::new(Semaphore::new(1)));
        let greeting =
            greet::<MockStream>(&budget, async { Err(ImapError::Io(refused).into()) }).await;
        let (kept, steps) = seen(greeting);
        assert!(kept.is_none());
        assert_eq!(
            steps,
            [format!(
                "unavailable ManageSieve {:?}",
                FailureClass::Retryable
            )]
        );
    }
}
