//! The probe of an account's ManageSieve service as `ImapAccount::connect` starts it, against the
//! loopback IMAP server.

use engine_core::ids::MailboxId;
use engine_provider::Provider;
use tokio::time::Instant;

use super::{
    tests::{Steps, account, observer},
    *,
};

/// An account on the loopback IMAP server whose ManageSieve endpoint takes the connection and
/// never greets, its observer, what that saw, and the endpoint's task.
async fn silent_account() -> (
    crate::ImapAccount<TlsStream<TcpStream>>,
    Arc<dyn ConnectObserver>,
    Steps,
    JoinHandle<()>,
) {
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = silent.local_addr().unwrap().to_string();
    let held = tokio::spawn(async move {
        let mut kept = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            kept.push(stream);
        }
    });
    let (cert, imap) = crate::test_server::gmail().await;
    let tls = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert])).unwrap();
    let (observer, steps) = observer();
    let config = ImapConfig::new(
        imap,
        "127.0.0.1",
        engine_provider::Credential::password("a", "b"),
    )
    .with_managesieve(addr, "localhost")
    .with_connect_observer(Arc::clone(&observer));
    let started = std::time::Instant::now();
    let connected = crate::ImapAccount::connect(&config, tls.connector())
        .await
        .expect("connects");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "the connect waited: {took:?}"
    );
    (connected, observer, steps, held)
}

// On the real clock until the account is connected: a paused one would run the IMAP dial's own
// deadlines out while it waits on the loopback server's sockets.
#[tokio::test]
async fn an_endpoint_that_never_greets_does_not_delay_the_connect() {
    let (connected, _, steps, held) = silent_account().await;
    let provider = connected.provider(MailboxId::try_from("INBOX").unwrap());
    let rules = provider.mail_rules().expect("not known yet: offered");
    // The probe is the one timer left: a paused clock runs it out at once.
    tokio::time::pause();
    let waiting = Instant::now();
    let err = rules.rule_vocabulary(&account()).await.unwrap_err();
    // The timer wheel rounds up to the next millisecond.
    let waited = waiting.elapsed();
    assert!(
        waited <= PROBE_DEADLINE + Duration::from_millis(5),
        "{waited:?}"
    );
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(provider.mail_rules().is_none());
    assert_eq!(sieve_steps(&steps), ["unavailable ManageSieve Retryable"]);
    held.abort();
}

/// The steps the observer saw of the ManageSieve service.
fn sieve_steps(steps: &Steps) -> Vec<String> {
    let steps = steps.lock().unwrap();
    steps
        .iter()
        .filter(|step| step.contains("ManageSieve"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn an_account_s_probe_ends_with_the_account() {
    let (connected, observer, steps, held) = silent_account().await;
    let provider = connected.provider(MailboxId::try_from("INBOX").unwrap());
    assert!(
        Arc::strong_count(&observer) > 1,
        "the account and its probe hold it"
    );
    drop(provider);
    drop(connected);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    // Only this test holds the observer now: the pool, the service and the probe's task are
    // gone, though the probe never landed.
    assert_eq!(Arc::strong_count(&observer), 1);
    assert_eq!(sieve_steps(&steps), Vec::<String>::new());
    held.abort();
}
