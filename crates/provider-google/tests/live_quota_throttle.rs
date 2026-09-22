//! Gated live proof that Gmail's `403` quota refusal is *waited out* rather than returned.
//!
//! This is the live half of #218. Nothing offline can
//! stand in for it: the fakes answer canned bytes whatever they are sent, so a test built on
//! one proves that the classifier reads a body correctly and says nothing about whether
//! Gmail still refuses in the shape the fixture was captured in — or whether the funnel's
//! wait is long enough to outlast a real quota window.
//!
//! # What it does
//!
//! Drains the whole mailbox over and over at the width the adapter really uses. One pass
//! over this account costs about 720 cost units (142 messages at 5 apiece, plus the listing),
//! and Gmail allows 6,000 per minute per user, so the ninth pass or so meets the wall. That
//! is the event under test:
//!
//! - the observer must record a **`403`**, which only happens if `GoogleThrottles` recognised the
//!   body — before #218 the status was read as "not allowed" and nothing was ever reported;
//! - and the pass that met it must still come back **`Ok`**, which only happens if the funnel
//!   waited and sent again. Before #218 the `403` reached the caller, the scope's sync failed, and
//!   the work waited for the next pass.
//!
//! To watch it fail, take `.classifying(…)` out of `HttpTransport::new` and run it again: the
//! log goes empty and the drain returns the rate limit.
//!
//! ⚠️ **It deliberately exhausts a minute of the account's quota**, so anything else pointed
//! at the same account for the next minute will meet a `403` that has nothing to do with it.
//! Run it alone.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_quota_throttle -- --nocapture
//! ```

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use engine_core::ids::AccountId;
use engine_http::{RetryConfig, ThrottleEvent};
use engine_provider::Provider;
use provider_google::{GmailProvider, GoogleClient};

/// Passes over the mailbox before giving up on provoking the limit. Eight should do it; the
/// headroom is for an account smaller than the one this was written against.
const MAX_PASSES: usize = 30;

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// One reported throttle, flattened to what this asserts on.
#[derive(Debug, Clone, Copy)]
struct Reported {
    status: u16,
    stated: Option<Duration>,
    gave_up: bool,
}

#[tokio::test]
async fn live_a_gmail_quota_refusal_is_waited_out_and_reported() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_gmail_quota_refusal_is_waited_out_and_reported: \
                   GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };

    let log: Arc<Mutex<Vec<Reported>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let retry = RetryConfig::default().with_observer(Arc::new(move |e: &ThrottleEvent<'_>| {
        sink.lock().expect("log").push(Reported {
            status: e.status,
            stated: e.stated,
            gave_up: e.gave_up,
        });
    }));
    let client = GoogleClient::connect(token, &engine_tls::TlsClientConfig::bundled(), &retry)
        .expect("client");
    let provider = GmailProvider::new(client);

    let started = Instant::now();
    let mut passes = 0;
    let mut messages = 0;
    while log.lock().expect("log").is_empty() && passes < MAX_PASSES {
        let outcome = provider.sync_email(&account(), None).await;
        passes += 1;
        let throttled = log.lock().expect("log").clone();
        match outcome {
            Ok(sync) => {
                if let engine_core::sync::SyncUpdate::Snapshot { objects, .. } = &sync.update {
                    messages = objects.len();
                }
            }
            Err(err) => panic!(
                "pass {passes} failed after {:?}: {err}\nreported: {throttled:?}\n\
                 A quota refusal reaching the caller is exactly the bug #218 describes — \
                 either the classifier did not recognise it, or the wait it asked for was \
                 declined by the policy's budget.",
                started.elapsed(),
            ),
        }
    }

    let reported = log.lock().expect("log").clone();
    println!(
        "{passes} pass(es) of {messages} message(s) in {:?}; {} throttle(s) reported",
        started.elapsed(),
        reported.len(),
    );
    for event in &reported {
        println!("  {event:?}");
    }

    assert!(
        !reported.is_empty(),
        "{passes} passes over {messages} messages did not reach Gmail's 6,000 units per \
         minute. Either the account is far smaller than the one this was written against, \
         or the quota was raised — check by hand before trusting a green run here, because \
         a limit that is never reached cannot prove anything was waited out.",
    );
    let quota: Vec<_> = reported.iter().filter(|e| e.status == 403).collect();
    assert!(
        !quota.is_empty(),
        "throttles were reported but none was a 403: {reported:?}. That is the *concurrency* \
         limit (Gmail's 429), which the status rule has always handled; this suite exists for \
         the quota refusal, which only a classifier can see.",
    );
    assert!(
        quota.iter().any(|e| !e.gave_up),
        "every quota refusal was given up on rather than waited out: {quota:?}",
    );
    assert!(
        started.elapsed() < Duration::from_mins(5),
        "the run outlasted any plausible quota window, which means it was waiting on \
         something else",
    );
    assert!(
        quota.iter().any(|e| e.stated.is_some()),
        "no quota refusal named its own window. Gmail states `window_start_time` in the \
         refusal's `details`; if that has stopped arriving, the wait is now a blind backoff \
         against a minute-long window and `throttle.rs` needs re-measuring.",
    );
}
