//! The probe of an account's ManageSieve service: what it reads, what it records, and that
//! nothing but a call to the service ever waits for it.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

use engine_core::ids::{AccountId, MailboxId};
use engine_provider::Provider;
use tokio::time::Instant;

use super::*;
use crate::{
    error::ImapError,
    managesieve::{
        Offer, Sieve,
        probe_state::{PROBE_BACKOFF, ProbeState},
        test_support::{GREETING, listing, scripted},
    },
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

/// What the observer saw, one line per step.
pub(super) type Steps = Arc<Mutex<Vec<String>>>;

pub(super) fn observer() -> (Arc<dyn ConnectObserver>, Steps) {
    let steps: Steps = Arc::default();
    let seen = Arc::clone(&steps);
    let observer = move |step: &ConnectStep<'_>| {
        seen.lock().unwrap().push(match step {
            ConnectStep::Unavailable { service, class, .. } => {
                format!("unavailable {service:?} {class:?}")
            }
            ConnectStep::Negotiated {
                dialect, features, ..
            } => format!("{dialect} {}", features.len()),
            _ => "other".to_owned(),
        });
    };
    (Arc::new(observer), steps)
}

/// What `probed` is recorded as, and what the observer saw.
fn landed(probed: SieveResult<Capabilities>) -> (Offer, bool, Vec<String>) {
    let state = Arc::new(ProbeState::unprobed());
    let (observer, steps) = observer();
    land(state.begin().unwrap(), observer.as_ref(), probed);
    let steps = steps.lock().unwrap().clone();
    (state.offered(), state.wanted(), steps)
}

#[tokio::test(start_paused = true)]
async fn a_greeting_is_read_with_no_credential_and_left_without_waiting() {
    // The server answers nothing to `LOGOUT`: the probe does not wait for it, not even for a
    // read's stall bound (time is paused, so any wait shows as elapsed time).
    let (stream, recorded_bytes) = MockStream::silent_after(GREETING.to_vec());
    let started = Instant::now();
    let greeting = greet(Session::open(stream).await.unwrap()).await;
    assert_eq!(started.elapsed(), Duration::ZERO, "waited for LOGOUT");
    assert_eq!(
        crate::mock::written(&recorded_bytes),
        "LOGOUT\r\n",
        "no credential sent"
    );
    let (offer, retried, steps) = landed(greeting);
    assert_eq!(offer, Offer::Found { reply: true });
    assert!(!retried);
    assert_eq!(
        steps,
        [format!("ManageSieve {}", capabilities_count(GREETING))]
    );
}

#[test]
fn a_failure_offers_nothing_and_one_that_may_pass_is_probed_again() {
    let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "x.example");
    let (offer, retried, steps) = landed(Err(ImapError::Io(refused).into()));
    assert_eq!((offer, retried), (Offer::Missing { may_pass: true }, true));
    assert_eq!(steps, ["unavailable ManageSieve Retryable"]);
    // No STARTTLS, or no ManageSieve at all: not offered, and not probed again.
    let (offer, retried, steps) = landed(Err(ImapError::protocol("no STARTTLS").into()));
    assert_eq!(
        (offer, retried),
        (Offer::Missing { may_pass: false }, false)
    );
    assert_eq!(steps, ["unavailable ManageSieve Permanent"]);
    // Offered without the reply where `vacation` is not listed.
    let (offer, ..) = landed(Ok(Capabilities::default()));
    assert_eq!(offer, Offer::Found { reply: false });
}

/// A greeting that takes `after` and then answers `answer`, counting how often it is read.
fn greeting(after: Option<Duration>, reply: bool, count: Arc<AtomicUsize>) -> Greet {
    Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let Some(after) = after else {
                // Blackholed: it takes the connection and never greets.
                return std::future::pending().await;
            };
            tokio::time::sleep(after).await;
            let mut capabilities = Capabilities::default();
            if reply {
                capabilities.sieve.push("vacation".to_owned());
            }
            Ok(capabilities)
        })
    })
}

/// What each session of a scripted service was sent.
type Sessions = Arc<Mutex<Vec<crate::mock::Recorded>>>;

/// A service whose one session lists no script, probed by `greet`, and what its observer saw.
fn probed(greet: Greet) -> (Arc<ManageSieve<MockStream>>, Steps, Sessions) {
    let (sieve, sessions) = scripted(&[&listing(&[])]);
    let (observer, steps) = observer();
    let sieve = Arc::new(sieve.probed_by(Prober::new(greet, Some(observer))));
    (sieve, steps, sessions)
}

/// A provider offering `sieve`'s rules and reply, as an account's providers do.
async fn provider_of(sieve: &Arc<ManageSieve<MockStream>>) -> crate::ImapProvider<MockStream> {
    let (stream, _) = MockStream::new(b"* OK ready\r\n".to_vec());
    let connection = crate::transport::Connection::open(stream).await.unwrap();
    crate::ImapProvider::with_connection(connection, MailboxId::try_from("INBOX").unwrap())
        .with_sieve(Some(Arc::clone(sieve) as Arc<dyn Sieve>))
}

/// Whether `provider` offers the rules and the reply.
fn offered(provider: &crate::ImapProvider<MockStream>) -> (bool, bool) {
    // Rules are imported wherever they are offered.
    #[cfg(feature = "import")]
    assert_eq!(
        provider.rules_importer().is_some(),
        provider.mail_rules().is_some()
    );
    (
        provider.mail_rules().is_some(),
        provider.vacation().is_some(),
    )
}

pub(super) fn account() -> AccountId {
    AccountId::try_from("a").unwrap()
}

/// A greeting that refuses as no ManageSieve server over TLS does: a definite answer.
fn refusing() -> Greet {
    Arc::new(|| Box::pin(async { Err(ImapError::protocol("no STARTTLS").into()) }))
}

#[tokio::test(start_paused = true)]
async fn while_the_first_probe_runs_both_are_offered_and_a_call_waits_for_it_to_land() {
    let (sieve, steps, sessions) = probed(greeting(
        Some(Duration::from_secs(3)),
        false,
        Arc::default(),
    ));
    let provider = provider_of(&sieve).await;
    assert_eq!(offered(&provider), (false, false), "no probe yet: nothing");
    let started = Instant::now();
    sieve.probe_now();
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "nothing waits to start it"
    );
    assert_eq!(offered(&provider), (true, true), "not known yet: offered");
    let rules = provider.mail_rules().unwrap();
    let read = rules
        .read_rules(&account())
        .await
        .expect("read once it landed");
    assert_eq!(read.rules.len(), 0);
    assert_eq!(started.elapsed(), Duration::from_secs(3), "the call waited");
    assert_eq!(sessions.lock().unwrap().len(), 1, "then dialled");
    assert_eq!(offered(&provider), (true, false), "no vacation listed");
    assert_eq!(*steps.lock().unwrap(), ["ManageSieve 0"]);
}

#[tokio::test(start_paused = true)]
async fn a_reply_call_waiting_for_a_probe_that_finds_no_vacation_dials_nothing() {
    let (sieve, _, sessions) = probed(greeting(
        Some(Duration::from_secs(3)),
        false,
        Arc::default(),
    ));
    let provider = provider_of(&sieve).await;
    sieve.probe_now();
    let reply = provider.vacation().expect("not known yet: offered");
    let err = reply.read_vacation(&account()).await.unwrap_err();
    assert!(err.is_item_unavailable(), "{err:?}");
    assert!(sessions.lock().unwrap().is_empty(), "no sign-in for it");
    assert!(provider.vacation().is_none());
}

#[tokio::test(start_paused = true)]
async fn a_probe_with_no_answer_makes_a_waiting_call_retryable_and_hides_both() {
    let (sieve, steps, sessions) = probed(greeting(None, true, Arc::default()));
    let provider = provider_of(&sieve).await;
    sieve.probe_now();
    let reply = provider.vacation().expect("not known yet: offered");
    let started = Instant::now();
    let err = reply.read_vacation(&account()).await.unwrap_err();
    assert_eq!(
        started.elapsed(),
        PROBE_DEADLINE,
        "the probe's deadline, no more"
    );
    assert_eq!(err.class(), FailureClass::Retryable, "it may answer later");
    assert!(!err.is_item_unavailable());
    assert!(
        sessions.lock().unwrap().is_empty(),
        "nothing dialled for it"
    );
    assert_eq!(
        offered(&provider),
        (false, false),
        "hidden until a probe succeeds"
    );
    assert_eq!(
        *steps.lock().unwrap(),
        ["unavailable ManageSieve Retryable"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_definite_answer_makes_a_call_item_unavailable() {
    let (sieve, steps, sessions) = probed(refusing());
    let provider = provider_of(&sieve).await;
    sieve.probe_now();
    let rules = provider.mail_rules().expect("not known yet: offered");
    let err = rules.read_rules(&account()).await.unwrap_err();
    assert!(
        err.is_item_unavailable(),
        "not offered, told apart: {err:?}"
    );
    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(sessions.lock().unwrap().is_empty());
    assert_eq!(offered(&provider), (false, false));
    assert_eq!(
        *steps.lock().unwrap(),
        ["unavailable ManageSieve Permanent"]
    );
    // A call through a reference taken before is refused at once, and no probe is wanted.
    let started = Instant::now();
    assert!(
        rules
            .rule_vocabulary(&account())
            .await
            .unwrap_err()
            .is_item_unavailable()
    );
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(!sieve.probe.wanted());
}

#[tokio::test(start_paused = true)]
async fn a_dial_starts_a_wanted_probe_without_waiting_once_per_backoff() {
    let count = Arc::new(AtomicUsize::new(0));
    let (sieve, steps, _) = probed(greeting(None, true, Arc::clone(&count)));
    sieve.probe_now();
    tokio::task::yield_now().await;
    assert_eq!(
        sieve.probe.settled().await,
        Offer::Missing { may_pass: true }
    );
    let started = Instant::now();
    // It failed in a way that may pass: wanted again, after the backoff.
    sieve.reprobe();
    tokio::task::yield_now().await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "backing off");
    tokio::time::advance(PROBE_BACKOFF).await;
    sieve.reprobe();
    sieve.reprobe();
    tokio::task::yield_now().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "one probe, not one per dial"
    );
    // Until it lands, the last finding stands, and nothing waits for it.
    assert_eq!(sieve.probe.offered(), Offer::Missing { may_pass: true });
    assert_eq!(
        sieve.probe.settled().await,
        Offer::Missing { may_pass: true }
    );
    assert_eq!(
        started.elapsed(),
        PROBE_BACKOFF,
        "a dial and a call never wait"
    );
    tokio::time::sleep(PROBE_DEADLINE).await;
    tokio::task::yield_now().await;
    assert_eq!(steps.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_reprobe_answers_from_the_last_finding_until_it_lands() {
    let answered = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&answered);
    // The first greeting answers at once without `vacation`; every later one never answers.
    let greet: Greet = Arc::new(move || {
        let first = count.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if !first {
                std::future::pending::<()>().await;
            }
            Ok(Capabilities::default())
        })
    });
    let (sieve, _, sessions) = probed(greet);
    let provider = provider_of(&sieve).await;
    sieve.probe_now();
    assert_eq!(sieve.probe.settled().await, Offer::Found { reply: false });
    sieve.probe.want();
    sieve.reprobe();
    tokio::task::yield_now().await;
    assert_eq!(answered.load(Ordering::SeqCst), 2, "the reprobe runs");
    assert_eq!(offered(&provider), (true, false), "the reply stays hidden");
    let started = Instant::now();
    provider
        .mail_rules()
        .unwrap()
        .read_rules(&account())
        .await
        .expect("served at once");
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "the reprobe is not waited for"
    );
    assert_eq!(sessions.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_older_probe_s_handle_never_replaces_a_newer_one() {
    let (sieve, ..) = probed(greeting(None, true, Arc::default()));
    let held = Arc::new(());
    let running = |held: Option<Arc<()>>| {
        ProbeTask(Some(tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(held);
        })))
    };
    // The newer probe's dial reaches `keep` first.
    sieve.keep(2, running(Some(Arc::clone(&held))));
    sieve.keep(1, running(None));
    tokio::task::yield_now().await;
    assert_eq!(Arc::strong_count(&held), 2, "the newer probe still runs");
    drop(sieve);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert_eq!(Arc::strong_count(&held), 1, "ended with its service");
}

#[tokio::test(start_paused = true)]
async fn dropping_the_service_ends_its_probe() {
    let (sieve, steps, _) = probed(greeting(None, true, Arc::default()));
    sieve.probe_now();
    tokio::task::yield_now().await;
    let state = Arc::downgrade(&sieve.probe);
    drop(sieve);
    // The task held the probe state; aborted, it lets go of it without recording anything.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(
        state.upgrade().is_none(),
        "the probe's task outlived its service"
    );
    assert_eq!(*steps.lock().unwrap(), Vec::<String>::new());
}
