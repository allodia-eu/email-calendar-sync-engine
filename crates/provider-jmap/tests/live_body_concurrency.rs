//! Gated live measurement of what overlapping body downloads buys on JMAP.
//!
//! A JMAP page of metadata is one `Email/get` for the whole page — the protocol batches it —
//! so the snapshot has no per-message round trip to hide. The **body** does: a message's raw
//! source is a blob download from the session's `downloadUrl`, one `GET` per message, and
//! `Provider::fetch_message_source` is one message wide. A host warming a mailbox therefore
//! pays one round trip per message however fast the link is, which is the same shape the
//! Gmail page fetch had.
//!
//! This prints rather than asserts a rate. Against the local harness the round trip is
//! sub-millisecond, so nothing here is the number a real server gives: the sweep exists to
//! show where a server's own ceiling is. What it *asserts* is that a drain at the width the
//! adapter actually uses — the session's `maxConcurrentRequests` — comes back clean, because
//! that is the claim `ConnectionInfo::concurrent_fetches` makes to a host. The wider steps
//! are deliberately past it and are expected to be refused; they are printed, not asserted.
//!
//! Skips without the harness, like every other live suite here:
//!
//! ```sh
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_body_concurrency -- --nocapture
//! ```

use std::time::Instant;

use engine_core::{ids::AccountId, mail::Message, sync::SyncUpdate};
use engine_provider::Provider;
use futures_util::{StreamExt, stream};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

/// The widths swept. `1` is the shipping behaviour and is measured twice — first and last —
/// so a serving-side warm-up cannot be read as a concurrency win.
const WIDTHS: [usize; 7] = [1, 2, 4, 8, 16, 32, 1];

/// Counts convert through `u32` because clippy's pedantic set denies a bare `usize as f64`.
fn count_as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

/// Fetches every message's source `width` at a time, returning the elapsed seconds, how many
/// came back, and the first failure — which is the whole point of sweeping past a server's
/// ceiling: a dropped body has to name itself, not vanish into a count.
async fn drain(
    provider: &JmapProvider,
    messages: &[Message],
    width: usize,
) -> (f64, usize, Option<String>) {
    let started = Instant::now();
    let outcomes: Vec<_> = stream::iter(messages)
        .map(|message| async move { provider.fetch_message_source(&account(), message).await })
        .buffered(width)
        .collect()
        .await;
    let failure = outcomes
        .iter()
        .find_map(|r| r.as_ref().err())
        .map(|e| format!("{:?}: {e}", e.class()));
    let fetched = outcomes.iter().filter(|r| r.is_ok()).count();
    (started.elapsed().as_secs_f64(), fetched, failure)
}

#[tokio::test]
async fn live_body_fetch_concurrency() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_body_fetch_concurrency: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    let provider = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("connect");

    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    let messages: Vec<Message> = objects
        .into_iter()
        .filter(|m| m.blob_id.is_some())
        .collect();
    assert!(
        !messages.is_empty(),
        "the harness should hold mail with downloadable bodies",
    );
    println!("{} message(s) with a blob to download", messages.len());

    let advertised = Provider::connection_info(&provider).concurrent_fetches;
    println!("the session grants {advertised} concurrent request(s)");

    for width in WIDTHS {
        let (seconds, fetched, failure) = drain(&provider, &messages, width).await;
        if width <= advertised {
            assert_eq!(
                fetched,
                messages.len(),
                "a {width}-wide drain is inside what the session granted and must not \
                 drop a body — {failure:?}",
            );
        }
        println!(
            "  {width:>2} in flight: {seconds:6.2}s  {:6.1} bodies/s  {fetched}/{} fetched{}",
            count_as_f64(fetched) / seconds,
            messages.len(),
            failure.map_or(String::new(), |f| format!("  — first failure {f}")),
        );
    }
}
