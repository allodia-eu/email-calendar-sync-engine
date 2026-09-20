//! Gated live proof that the account gate is what keeps a Graph mailbox under its
//! concurrency ceiling — and that the ceiling is real.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set, like every live suite here; see
//! `live_provider.rs` for the command. Read-only throughout: it pages folder lists.
//!
//! # Why both arms exist
//!
//! The claim this suite records is an **absence** — "gated at four, Graph refuses nothing" —
//! and `AGENTS.md` is explicit that an absence has to be proved against the request that asks
//! for it, or an adapter that simply never sent enough produces the same green. #102 is what
//! that costs when it is skipped.
//!
//! So the control arm sends *more*, through the same adapter, by making the one mistake this
//! whole mechanism exists to prevent: **a gate per provider instead of a gate per account.**
//! Two clients, two gates of four, eight requests in flight against one mailbox. That is not a
//! hypothetical error — it is the shape a host shipped, where the bound was a field on a token
//! source and the host built two of them in one process.
//!
//! If the control arm ever stops throttling, this suite is no longer evidence of anything and
//! the ceiling has moved; that is a failure worth seeing, so it asserts rather than skips.

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use common::{account, token};
use engine_http::{RequestGate, RetryConfig, ThrottleEvent, ThrottleObserver};
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider};

/// Counts throttles, and remembers the widest attempt number any one request reached.
#[derive(Default)]
struct CountThrottles {
    events: AtomicUsize,
    gave_up: AtomicUsize,
}

impl ThrottleObserver for CountThrottles {
    fn throttled(&self, event: &ThrottleEvent<'_>) {
        self.events.fetch_add(1, Ordering::SeqCst);
        if event.gave_up {
            self.gave_up.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// A folder-bound provider on `retry`, as a host builds one per folder.
fn provider(token: &str, retry: &RetryConfig) -> GraphProvider {
    let client = GraphClient::connect(token, &engine_tls::TlsClientConfig::bundled(), retry)
        .expect("client");
    GraphProvider::new(
        client,
        engine_core::ids::MailboxId::try_from("inbox").unwrap(),
    )
}

/// Runs `total` account-level folder-list syncs, `providers` of them round-robin, and
/// reports how many throttles the observer saw.
async fn folder_list_storm(providers: Vec<GraphProvider>, total: usize) -> usize {
    let providers = Arc::new(providers);
    let failures = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for index in 0..total {
        let providers = Arc::clone(&providers);
        let failures = Arc::clone(&failures);
        handles.push(tokio::spawn(async move {
            let provider = &providers[index % providers.len()];
            if provider.sync_mailboxes(&account(), None).await.is_err() {
                failures.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for handle in handles {
        handle.await.expect("task");
    }
    failures.load(Ordering::SeqCst)
}

/// One gate for the account — what a host is supposed to build — and the mailbox is content.
///
/// The adapter narrows it to Graph's documented per-mailbox four in `HttpTransport::new`, so
/// nothing here states a number: a test that set the ceiling itself would be asserting its own
/// arithmetic rather than the adapter's claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_one_gate_per_account_keeps_a_mailbox_under_its_ceiling() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_one_gate_per_account_keeps_a_mailbox_under_its_ceiling: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let observer = Arc::new(CountThrottles::default());
    let gate = RequestGate::new();
    let retry = RetryConfig::default()
        .with_observer(Arc::clone(&observer) as Arc<dyn ThrottleObserver>)
        .gated(gate.clone());
    // Four providers, as four of an account's folders would be — all sharing the one gate.
    let providers: Vec<_> = (0..4).map(|_| provider(&token, &retry)).collect();
    assert_eq!(
        gate.limit(),
        Some(4),
        "the adapter states Graph's per-mailbox ceiling as it connects",
    );

    let failures = folder_list_storm(providers, 16).await;

    assert_eq!(failures, 0, "a sync failed outright");
    assert_eq!(
        observer.events.load(Ordering::SeqCst),
        0,
        "Graph refused a request the account gate should have held back",
    );
}

/// The control arm: the same adapter, the same requests, a gate **per provider**.
///
/// This is what makes the arm above evidence. Without it, four providers that quietly sent
/// nothing concurrent would report the same clean run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_a_gate_per_provider_is_refused_by_the_same_mailbox() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_a_gate_per_provider_is_refused_by_the_same_mailbox: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let observer = Arc::new(CountThrottles::default());
    // Two providers, each with its own gate of four: eight in flight against one mailbox,
    // which is the two-cores-two-semaphores shape exactly.
    let providers: Vec<_> = (0..2)
        .map(|_| {
            let retry = RetryConfig::default()
                .with_observer(Arc::clone(&observer) as Arc<dyn ThrottleObserver>)
                .gated(RequestGate::new());
            provider(&token, &retry)
        })
        .collect();

    folder_list_storm(providers, 16).await;

    assert!(
        observer.events.load(Ordering::SeqCst) > 0,
        "eight concurrent requests drew no throttle — the ceiling moved, and the arm above \
         no longer proves anything",
    );
}
