//! Gated live proof that Stalwart's `400 maxConcurrentRequests` is *waited out*.
//!
//! The JMAP half of #218. The adapter has called this
//! refusal a rate limit since it was first observed, and then handed it back — so a draft
//! save that met it failed, and the outbox op waited for another pass. Now the shared send
//! funnel waits and sends again.
//!
//! # Finding the refusal at all
//!
//! It is not where the comment that recorded it implies. Measured on this harness
//! (v0.16.21, `maxConcurrentRequests: 4`):
//!
//! | Offered | Result |
//! |---|---|
//! | 300 simultaneous `Core/echo` API calls | `200` throughout |
//! | 300 simultaneous blob **downloads** | `200` throughout |
//! | 16 simultaneous blob **uploads** (64 KB) | 6 refused `400` |
//!
//! So Stalwart enforces the number on uploads and nothing else here, and reports it as
//! `maxConcurrentRequests` even though the session advertises a separate
//! `maxConcurrentUpload`. That is why this test saves drafts **with attachments**: an
//! attachment is the adapter's only upload, and without one there is no refusal to wait out.
//!
//! # Why four providers
//!
//! One would not do. `JmapClient::connect` narrows that account's
//! [`RequestGate`](engine_http::RequestGate) to the session's `maxConcurrentRequests`, so a
//! single provider cannot exceed the limit and cannot reach the refusal — which is the gate
//! working, not the classifier being unnecessary. Four providers are four gates, and four
//! gates are what the user's other devices are: a ceiling bounds one account *in this
//! process*, and the server counts everyone. That is the case this test is about, and the
//! only case in which the refusal is reachable at all.
//!
//! Each attachment is 64 KB, so a run stores about 1 MB against the harness's 50 MB blob
//! quota, and every draft is removed at the end. Firing 4 MB uploads instead — the first
//! thing that worked — exhausts that quota in one run and answers `429 Quota exceeded` for
//! the next three quarters of an hour.
//!
//! Skips with no `STALWART_HTTP_ADDR`, like every live suite here:
//!
//! ```sh
//! scripts/ci/stalwart-live.sh up
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-jmap --test live_concurrency_limit -- --nocapture
//! ```

use std::sync::{Arc, Mutex};

use engine_core::{
    ids::{AccountId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_http::{RetryConfig, ThrottleEvent};
use engine_provider::{Draft, DraftAttachment, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

/// Independent clients of one account — four of the user's devices, in effect. Each gets
/// its own gate, so together they can offer more than the account's ceiling.
const CLIENTS: usize = 4;
/// Drafts each client saves at once, so `CLIENTS * PER_CLIENT` uploads are offered per
/// round.
const PER_CLIENT: usize = 6;
/// Rounds to offer before giving up on provoking a refusal.
///
/// More than one because the provocation is not reliable in a single round: a raw sweep
/// refuses 6 of 16 every time, but through the adapter each upload is separated by the
/// `Email/set` that follows it, so a round can slip through with nothing overlapping. Three
/// runs of one round each went 3 refusals, 0, 0 — which as a CI test is a coin toss, and a
/// red build that means nothing is worse than no test. Each round is cleaned up before the
/// next, so the blob quota sees one round's worth at a time.
const ROUNDS: usize = 8;
/// Big enough to stay in flight while its neighbours arrive, small enough that a run costs
/// a fiftieth of the blob quota.
const ATTACHMENT_BYTES: usize = 64 * 1024;

fn account() -> AccountId {
    AccountId::try_from("live-concurrency").unwrap()
}

/// One reported throttle, flattened to what this asserts on.
#[derive(Debug, Clone, Copy)]
struct Reported {
    status: u16,
    gave_up: bool,
}

fn draft(n: usize) -> Draft {
    Draft::new(
        MessageIdHeader::new(format!("<concurrency-{n}@test.local>")).unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        format!("Concurrency probe {n}"),
        "Saved by the live concurrency test (never submitted).",
    )
    .with_attachment(DraftAttachment::attachment(
        "filler.bin",
        "application/octet-stream",
        vec![b'x'; ATTACHMENT_BYTES],
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_a_jmap_concurrency_refusal_is_waited_out_rather_than_returned() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping live_a_jmap_concurrency_refusal_is_waited_out_rather_than_returned: \
             STALWART_HTTP_ADDR unset"
        );
        return;
    };

    let log: Arc<Mutex<Vec<Reported>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let observer: Arc<dyn engine_http::ThrottleObserver> =
        Arc::new(move |e: &ThrottleEvent<'_>| {
            sink.lock().expect("log").push(Reported {
                status: e.status,
                gave_up: e.gave_up,
            });
        });

    let mut providers = Vec::new();
    for _ in 0..CLIENTS {
        // A fresh `RetryConfig` per client, so each carries its own gate — the whole point.
        // They share the observer, so one log sees every wait.
        let config = JmapConfig::new(
            format!("http://{}", harness.http_addr),
            Credentials::basic(&harness.account, &harness.password),
        )
        .with_retry(RetryConfig::default().with_observer(Arc::clone(&observer)));
        providers.push(JmapProvider::connect(config).await.expect("connect"));
    }
    assert_eq!(
        providers[0].connection_info().concurrent_fetches,
        4,
        "this harness is configured differently from the one the widths were measured on",
    );

    let mut rounds = 0;
    let mut offered = 0;
    while rounds < ROUNDS {
        rounds += 1;
        let saved: Vec<_> = futures_util::future::join_all(providers.iter().enumerate().flat_map(
            |(client, provider)| {
                (0..PER_CLIENT).map(move |n| {
                    let draft = draft(rounds * 1_000 + client * PER_CLIENT + n);
                    async move { provider.put_draft(&account(), &draft, None).await }
                })
            },
        ))
        .await;
        offered += saved.len();

        let reported = log.lock().expect("log").clone();
        let failures: Vec<_> = saved.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(
            failures.is_empty(),
            "every draft must survive the refusal: {failures:?}\nreported: {reported:?}\n\
             A `RateLimited` here is #218 exactly: the adapter named the refusal correctly \
             and nothing waited for it.",
        );

        // Leave the mailbox as this round found it before offering another — the harness is
        // shared with every other live suite, and a freed blob is quota back.
        for (provider, key) in providers.iter().cycle().zip(saved.into_iter().flatten()) {
            provider
                .delete_draft(&account(), &key)
                .await
                .expect("cleanup");
        }
        if reported.iter().any(|e| e.status == 400) {
            break;
        }
    }

    let reported = log.lock().expect("log").clone();
    let refused = reported.iter().filter(|e| e.status == 400).count();
    println!(
        "{offered} upload(s) offered over {rounds} round(s); {refused} concurrency refusal(s) \
         waited out, {} event(s) total",
        reported.len(),
    );
    assert!(
        refused > 0,
        "{offered} simultaneous uploads over {rounds} rounds drew no refusal, so nothing was \
         waited out and this proved nothing. A raw sweep refuses 6 of 16 on this harness; if \
         Stalwart has stopped enforcing `maxConcurrentRequests`, say so in the PR rather than \
         deleting the test.",
    );
    assert!(
        reported
            .iter()
            .filter(|e| e.status == 400)
            .all(|e| !e.gave_up),
        "a refusal was given up on rather than waited out: {reported:?}",
    );
}
