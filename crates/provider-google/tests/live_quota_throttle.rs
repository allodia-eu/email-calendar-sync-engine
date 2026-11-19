//! Gated live proof of what the engine does with Gmail's `403` quota refusal: it names the
//! instant the window reopens, and coming back then works.
//!
//! The live half of #218 and its follow-up. Nothing offline can stand in for it: the fakes
//! answer canned bytes whatever they are sent, so a test built on one proves the classifier
//! reads a body correctly and says nothing about whether Gmail still refuses in the shape the
//! fixture was captured in — or whether the instant it names is one you can actually schedule
//! against.
//!
//! # The contract under test
//!
//! The adapter paces itself under the quota (`live_quota_pace.rs`), so its own spending never
//! meets the wall. What still does is **another client of the same user** spending the same
//! budget: the app on a second device, under the same Cloud project. This plays that client,
//! with an unpaced twenty-wide burst of message fetches (20 units apiece against 6,000 per
//! minute) until Gmail refuses it, and then runs the adapter's pass into the drained window.
//! Then, in order:
//!
//! 1. the pass **fails** rather than parking — a quota window is most of a minute and the engine
//!    does not sleep a task, or an account's gate lane, through one;
//! 2. the failure is **`RateLimited`** rather than the permission error a bare `403` reads as —
//!    which the adapter has got right since it landed, so this step documents rather than guards;
//! 3. it **names when**, read out of the refusal's `window_start_time`, since Gmail sends no
//!    `Retry-After` on either of its refusals — **this is the step that goes red**, because the
//!    instant only exists if the shared funnel recognised the reply as a throttle;
//! 4. and **honouring that instant works** — the whole point, and the step that would fail if the
//!    number were a guess dressed up as the server's word.
//!
//! Step 4 is the host's job in production. Here the test plays the host, which is the honest
//! way to test a contract whose other half lives outside the engine.
//!
//! Verified by taking `.classifying(…)` out of `HttpTransport::new`: step 3 fails. Note what
//! step 2 does **not** catch there — the class stays `RateLimited`, because the adapter's own
//! reading of the body was never the broken part. That is #218's whole point, and it is why
//! the class alone was never enough to act on.
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

use engine_core::{error::FailureClass, ids::AccountId};
use engine_http::{RetryConfig, ThrottleEvent};
use engine_provider::Provider;
use provider_google::{GmailProvider, GoogleClient};

/// Drained windows before giving up on a refusal that names its instant.
const MAX_PASSES: usize = 12;

/// Rounds of a hundred fetches the outside client spends before giving up on a refusal. At 20
/// units a fetch, Gmail's 6,000 a minute is gone in four; fifty is far past any quota this test
/// expects, so a run that reaches it says so rather than spending forever.
const MAX_ROUNDS: usize = 50;

/// The Gmail API root the outside client calls directly.
const GMAIL: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Added to the instant the server named before coming back.
///
/// Not impatience insurance — the window's end is computed against *this device's* clock,
/// and a second of skew either way would otherwise land the retry just inside a window that
/// has not reopened, which reads exactly like the instant being wrong.
const MARGIN: Duration = Duration::from_secs(2);

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// Spends the user's quota the way another, unpaced client would: twenty message fetches at a
/// time until Gmail refuses one.
async fn spend_the_quota_elsewhere(token: &str) {
    use futures_util::{StreamExt, stream};
    let http = reqwest::Client::new();
    let listed: serde_json::Value = http
        .get(format!("{GMAIL}/messages?maxResults=100"))
        .bearer_auth(token)
        .send()
        .await
        .expect("list")
        .json()
        .await
        .expect("list body");
    let ids: Vec<String> = listed["messages"]
        .as_array()
        .expect("the test mailbox has messages")
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .collect();
    for _ in 0..MAX_ROUNDS {
        let statuses: Vec<u16> = stream::iter(ids.iter())
            .map(|id| {
                let request = http
                    .get(format!("{GMAIL}/messages/{id}?format=metadata"))
                    .bearer_auth(token);
                async move { request.send().await.map_or(0, |r| r.status().as_u16()) }
            })
            .buffer_unordered(20)
            .collect()
            .await;
        if statuses.contains(&403) {
            return;
        }
    }
    panic!(
        "{MAX_ROUNDS} rounds of fetches drew no quota refusal: the project's quota is larger than \
         Gmail's published 6,000 units a minute, or every fetch failed before reaching it"
    );
}

/// One reported throttle, flattened to what this asserts on.
#[derive(Debug, Clone, Copy)]
struct Reported {
    status: u16,
    stated: Option<Duration>,
    gave_up: bool,
}

#[tokio::test]
async fn live_a_gmail_quota_refusal_names_when_it_clears_and_coming_back_then_works() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_gmail_quota_refusal_names_when_it_clears_and_coming_back_then_works: \
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
    let client = GoogleClient::connect(
        token.clone(),
        &engine_tls::TlsClientConfig::bundled(),
        &retry,
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let provider = GmailProvider::new(client);

    // --- 1..3: another client drains the window; the pass meets it, until a refusal names an
    // instant. ---
    let started = Instant::now();
    let mut passes = 0;
    let mut refusals = 0;
    let (refusal, named) = loop {
        assert!(
            passes < MAX_PASSES,
            "{passes} passes drew {refusals} refusal(s), none naming an instant. If refusals \
             came and none carried `window_start_time`, Gmail has stopped sending it and \
             `throttle.rs` needs re-measuring: about a third of refusals carried it when this \
             was written, so a dozen without is a finding, not a flake.",
        );
        passes += 1;
        spend_the_quota_elsewhere(&token).await;
        let Err(err) = provider.sync_email(&account(), None).await else {
            continue;
        };
        refusals += 1;
        assert_eq!(
            err.class(),
            FailureClass::RateLimited,
            "a `403` is Google's ordinary not-allowed status, and only the body separates \
             the two. This has held since the adapter landed; if it breaks, `error.rs` has \
             drifted from what Gmail sends, not the funnel",
        );
        if let Some(named) = err.retry_after() {
            break (err, named);
        }
        println!("  refusal {refusals} named no window (two thirds do not); asking again");
    };
    println!(
        "refused on pass {passes} ({refusals} refusal(s)) after {:?}: {refusal}",
        started.elapsed(),
    );

    let wait = Duration::new(named.seconds(), named.nanoseconds());
    println!("the server named {wait:?}");
    assert!(
        wait <= Duration::from_mins(1),
        "a per-minute window cannot reopen more than a minute out: {wait:?}",
    );

    let reported = log.lock().expect("log").clone();
    let quota: Vec<_> = reported.iter().filter(|e| e.status == 403).collect();
    assert!(
        quota.iter().any(|e| e.gave_up),
        "the host was not told about the pause it is now expected to schedule: {reported:?}",
    );
    assert!(
        quota.iter().any(|e| e.stated == Some(wait)),
        "the refusal named {wait:?} to the caller but the host's log was not told the \
         same figure — the two copies of the instant have drifted",
    );

    // --- 4: play the host, and come back when it said. ---
    tokio::time::sleep(wait + MARGIN).await;
    let recovered = provider.sync_email(&account(), None).await;
    assert!(
        recovered.is_ok(),
        "the window had not reopened when the server said it would, so the instant is not \
         one a host can schedule against: {:?}",
        recovered.err(),
    );
    println!(
        "recovered after honouring the instant; {:?} total",
        started.elapsed()
    );
}
