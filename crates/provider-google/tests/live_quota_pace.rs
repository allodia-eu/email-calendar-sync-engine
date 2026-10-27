//! Gated live proof that the adapter paces itself under Gmail's per-user quota: drained
//! back to back for over two minutes' worth of units, the mailbox draws no refusal at all.
//!
//! Gmail bills a message fetch at 20 units against 6,000 per user per minute, so one drain of
//! this account (about 190 messages) costs about 3,900, and three in a row are nearly twice
//! what a minute allows. Unpaced, the adapter's twenty-wide fan-out spends a minute's budget
//! in seconds and the second drain meets `403 rateLimitExceeded`. Paced, the drains take as
//! long as the budget says and nothing is refused.
//!
//! Verified by taking the `pace_to` out of `HttpTransport::new`: the second drain is refused.
//!
//! ⚠️ **Rest a minute before running it**, and run it alone: the claim is about *this* client's
//! spending, so units another suite spent in the same minute would read as a pace that failed.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_quota_pace -- --nocapture
//! ```

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use engine_core::ids::AccountId;
use engine_http::{RetryConfig, ThrottleEvent};
use engine_provider::Provider;
use provider_google::{GmailProvider, GoogleClient};

/// Drains in a row: enough that an unpaced client is refused, few enough to finish in minutes.
const DRAINS: usize = 3;

#[tokio::test]
async fn live_back_to_back_drains_stay_under_the_quota() {
    let Some(token) = std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    else {
        eprintln!(
            "skipping live_back_to_back_drains_stay_under_the_quota: GOOGLE_ACCESS_TOKEN unset"
        );
        return;
    };

    let refused: Arc<Mutex<Vec<u16>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&refused);
    let retry = RetryConfig::default().with_observer(Arc::new(move |e: &ThrottleEvent<'_>| {
        sink.lock().expect("log").push(e.status);
    }));
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &retry,
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let provider = GmailProvider::new(client);
    let account = AccountId::try_from("live").unwrap();

    let started = Instant::now();
    let mut messages = 0;
    for drain in 1..=DRAINS {
        let sync = provider
            .sync_email(&account, None)
            .await
            .unwrap_or_else(|err| panic!("drain {drain} was refused: {err}"));
        let engine_core::sync::SyncUpdate::Snapshot { objects, .. } = &sync.update else {
            panic!("a drain from no cursor is a snapshot");
        };
        messages += objects.len();
        println!(
            "drain {drain}: {} messages, {:?} in",
            objects.len(),
            started.elapsed()
        );
    }

    let refusals = refused.lock().expect("log").clone();
    assert!(
        refusals.is_empty(),
        "the paced client was throttled {} time(s): {refusals:?}",
        refusals.len(),
    );
    // About 20 units a message: more than a minute's budget must have taken more than a minute.
    let units = u32::try_from(messages).unwrap() * 20;
    println!(
        "{messages} messages, about {units} units, in {:?}",
        started.elapsed()
    );
    if units > 6000 {
        assert!(
            started.elapsed() > Duration::from_mins(1),
            "{units} units in {:?} is faster than Gmail allows, so the pace is not holding",
            started.elapsed(),
        );
    }
}
