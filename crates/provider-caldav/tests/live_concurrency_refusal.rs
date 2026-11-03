//! Gated live proof that Stalwart's DAV concurrency refusal is *waited out* (#332).
//!
//! Stalwart counts DAV requests against the same per-account cap as JMAP ones
//! (`maxConcurrentRequests`, 4 on this harness), and refuses one over it with a `400` and
//! JMAP's `urn:ietf:params:jmap:error:limit` body. Read by its status that is a request
//! defect, so `provider-caldav` used to report a create that would have succeeded a moment
//! later as `Permanent`. `live_caldav`'s concurrent tests met it now and then, as a flake.
//!
//! # Why this provokes it and `live_concurrency` never did
//!
//! That suite's arm A offers a fixed 25 requests a second, and at 25/s a request is long
//! finished before the next arrives, so whatever its width bound, about one is ever in flight.
//! This offers [`WIDTH`] creates **at once**, through one provider, whose account gate a DAV
//! adapter leaves unbounded: the case of a host fanning out a batch of writes, or of several
//! of the user's devices syncing together.
//!
//! Measured raw against v1.0.2 before the fix: 48 simultaneous create-only `PUT`s, 34 refused;
//! each refused resource then answered `GET` with `404`. So a refused write was never applied,
//! and sending it again cannot store it twice. The creates here carry `If-None-Match: *` as
//! every adapter create does, so a replay of one the server *had* applied would be answered
//! `412` and fail this test rather than pass it.
//!
//! Writes into a scratch account's calendar, never the seeded one whose event count other
//! suites assert, and deletes everything it made — concurrently, so the `DELETE` path meets the
//! cap as well.
//!
//! ```sh
//! scripts/ci/stalwart-live.sh up
//! STALWART_HTTP_ADDR=127.0.0.1:18080 \
//!   cargo test -p provider-caldav --all-features --test live_concurrency_refusal -- --nocapture
//! ```

use std::sync::{Arc, Mutex};

use engine_core::{
    ids::{AccountId, Uid},
    sync::SyncUpdate,
    time::{CalendarDateTime, UtcDateTime},
};
use engine_http::{RetryConfig, ThrottleEvent};
use engine_provider::{CalendarWrites, EventDeletion, EventDraft, Provider};
use provider_caldav::{CalDavConfig, CalDavProvider, Credentials};
use stalwart_harness::Harness;

/// Creates offered at once — six times the account's cap. A raw sweep of 48 drew 34
/// refusals; through the adapter each create is a single `PUT`, so the overlap is as close.
const WIDTH: usize = 24;
/// Rounds to offer before concluding the server no longer refuses. A round that slips
/// through clean is possible, and a coin-toss test is worse than none.
const ROUNDS: usize = 4;

fn account() -> AccountId {
    AccountId::try_from("live-dav-concurrency").unwrap()
}

/// One reported throttle, flattened to what this asserts on.
#[derive(Debug, Clone, Copy)]
struct Reported {
    status: u16,
    gave_up: bool,
}

fn at(local: &str) -> CalendarDateTime {
    CalendarDateTime::utc(local.parse().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_a_dav_concurrency_refusal_is_waited_out_rather_than_returned() {
    let Some(harness) = Harness::from_env() else {
        eprintln!(
            "skipping live_a_dav_concurrency_refusal_is_waited_out_rather_than_returned: \
             STALWART_HTTP_ADDR unset"
        );
        return;
    };
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");

    let log: Arc<Mutex<Vec<Reported>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let observer: Arc<dyn engine_http::ThrottleObserver> =
        Arc::new(move |e: &ThrottleEvent<'_>| {
            sink.lock().expect("log").push(Reported {
                status: e.status,
                gave_up: e.gave_up,
            });
        });

    let (user, pass) = harness.scratch[1].auth();
    let provider = CalDavProvider::connect(
        CalDavConfig::new(
            format!("http://{}", harness.http_addr),
            Credentials::Basic {
                username: user.to_owned(),
                password: pass.to_owned(),
            },
            engine_http::test_server::user_agent(),
        )
        .with_retry(RetryConfig::default().with_observer(observer)),
    )
    .await
    .expect("connect + discover");
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_calendars(&account(), None)
        .await
        .expect("list calendars")
        .sync
        .update
    else {
        panic!("a first calendar listing is a snapshot");
    };
    let calendar = objects
        .into_iter()
        .find(|c| c.id.as_str().trim_end_matches('/').ends_with("/default"))
        .expect("the scratch account has a default calendar")
        .id;

    let mut rounds = 0;
    let mut offered = 0;
    while rounds < ROUNDS {
        rounds += 1;
        let created = futures_util::future::join_all((0..WIDTH).map(|n| {
            let draft = EventDraft::new(
                calendar.clone(),
                Uid::new(format!("dav-concurrency-{rounds}-{n}@test.local")).unwrap(),
                format!("Concurrency probe {n}"),
                at("2026-12-01T10:00:00"),
                at("2026-12-01T11:00:00"),
                UtcDateTime::new(2026, 11, 3, 12, 0, 0).unwrap(),
            );
            let provider = &provider;
            async move { provider.create_event(&account(), &draft).await }
        }))
        .await;
        offered += created.len();

        let failures: Vec<_> = created.iter().filter_map(|r| r.as_ref().err()).collect();
        let deleted = futures_util::future::join_all(created.iter().flatten().map(|receipt| {
            let deletion = EventDeletion::unconditional(receipt.event.clone(), receipt.uid.clone());
            let provider = &provider;
            async move { provider.delete_event(&account(), None, &deletion).await }
        }))
        .await;
        let reported = log.lock().expect("log").clone();
        assert!(
            failures.is_empty(),
            "every create must survive the refusal: {failures:?}\nreported: {reported:?}\n\
             A `RateLimited` or `Permanent` here is #332 exactly: the server said \"not yet\" \
             and the adapter took it for \"no\".",
        );
        let undeleted: Vec<_> = deleted.iter().filter_map(|r| r.as_ref().err()).collect();
        assert!(undeleted.is_empty(), "cleanup: {undeleted:?}");
        if reported.iter().any(|e| e.status == 400) {
            break;
        }
    }

    let reported = log.lock().expect("log").clone();
    let refused = reported.iter().filter(|e| e.status == 400).count();
    println!(
        "{offered} create(s) offered over {rounds} round(s), each deleted again; {refused} \
         concurrency refusal(s) waited out, {} event(s) total",
        reported.len(),
    );
    assert!(
        refused > 0,
        "{offered} simultaneous creates over {rounds} rounds drew no refusal, so nothing was \
         waited out and this proved nothing. A raw sweep of 48 refused 34 on v1.0.2; if \
         Stalwart has stopped capping DAV concurrency, say so in the PR rather than deleting \
         the test.",
    );
    assert!(
        reported
            .iter()
            .filter(|e| e.status == 400)
            .all(|e| !e.gave_up),
        "a refusal was given up on rather than waited out: {reported:?}",
    );
}
