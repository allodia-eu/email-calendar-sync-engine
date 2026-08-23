//! Gated live measurement of how fast a Gmail snapshot actually drains.
//!
//! `messages.list` returns only `{id, threadId}`, so a snapshot's cost is one
//! `messages.get` per message and nothing else — the one adapter here with no companion
//! batch-get. That makes the pass's throughput the single number worth watching, and it is
//! not something an offline fake can tell you: the fakes answer instantly, so they prove
//! the fan-out *happens* (`fetch_tests`) but never what it *buys*.
//!
//! This prints rather than asserts a rate. A wall-clock threshold against someone else's
//! service, over whatever link the developer is on, is a flake generator; the number is for
//! a human comparing before and after a change to the fetch shape. The one thing it does
//! assert is that a concurrent drain comes back **clean** — no `429` — because that is the
//! property a wider window would break, and it is a real regression rather than a slow day.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set, like the other live suites:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_throughput -- --nocapture
//! ```
//!
//! For the API-level question underneath it — serial versus concurrent versus the batch
//! endpoint, payload size per `format`, gzip — use `tools/google-oauth/api-bench.sh`, which
//! probes Gmail directly rather than through the adapter.

use std::time::Instant;

use engine_core::{ids::AccountId, sync::SyncUpdate};
use engine_provider::Provider;
use provider_google::{GmailProvider, GoogleClient};

/// Counts convert through `u32` because clippy's pedantic set denies a bare `usize as f64`,
/// and no mailbox this probes is anywhere near saturating it.
fn count_as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// The bearer token, or `None` to skip the gated test.
fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

#[tokio::test]
async fn live_snapshot_throughput() {
    let Some(token) = token() else {
        eprintln!("skipping live_snapshot_throughput: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);

    let started = Instant::now();
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("a concurrent snapshot must not be throttled");
    let elapsed = started.elapsed();

    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("a first sync is a snapshot");
    };
    let count = objects.len();
    assert!(count > 0, "the live account should hold some mail");

    let seconds = elapsed.as_secs_f64();
    let per_message = elapsed.as_secs_f64() * 1000.0 / count_as_f64(count);
    println!(
        "snapshot: {count} message(s) in {seconds:.2}s — {per_message:.0} ms/message, \
         {:.1} messages/second",
        count_as_f64(count) / seconds,
    );
}
