//! Offline tests for the bounded connection pool, over scripted mock streams.
//!
//! The properties here are the ones a real server punishes and no unit under test can observe
//! on its own: how many sockets exist at once, whether a resting one is proved alive before
//! reuse, and whether watches can crowd out syncing. Each is asserted by counting *dials*,
//! because the dial is the socket — a pool that quietly opens one more is exactly the bug.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use engine_core::error::FailureClass;

use super::*;
use crate::mock::{MockStream, Recorded, script, written};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] ready\r\n";
/// Enough `NOOP` answers that a reused connection can be validated several times over. A
/// parked connection is checked on every hand-out, so a script that answers once would make a
/// third acquire look like a dead socket and quietly change what the test measures.
const NOOPS: &str = "a1 OK NOOP done\r\na2 OK NOOP done\r\na3 OK NOOP done\r\na4 OK NOOP done\r\n";
/// A rest threshold every parked connection has already reached, so each reuse is a checked one
/// — the behaviour most of these tests are about.
const ALWAYS_CHECK: Duration = Duration::ZERO;
/// A rest threshold no test outlives, so a reuse is never checked.
const NEVER_CHECK: Duration = Duration::from_hours(1);

/// A dial factory over mock streams, and the count of how many times it was called.
///
/// `healthy` scripts a connection that answers `NOOP`; `dead` scripts one whose stream is
/// exhausted, which is how a socket that died while resting behaves — it looks open until
/// something is written to it.
fn dialler(healthy: bool) -> (Dial<MockStream>, Arc<AtomicUsize>) {
    let (dial, dials, _written) = recording_dialler(healthy);
    (dial, dials)
}

/// [`dialler`], also keeping what the client wrote on every connection it dialled.
fn recording_dialler(
    healthy: bool,
) -> (
    Dial<MockStream>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<Recorded>>>,
) {
    let dials = Arc::new(AtomicUsize::new(0));
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&dials);
    let recordings = Arc::clone(&sessions);
    let dial: Dial<MockStream> = Arc::new(move || {
        counter.fetch_add(1, AtomicOrdering::SeqCst);
        let server = if healthy {
            script(&[GREETING, NOOPS])
        } else {
            script(&[GREETING])
        };
        let (stream, recorded) = MockStream::new(server);
        recordings.lock().unwrap().push(recorded);
        Box::pin(async move { Connection::open(stream).await })
    });
    (dial, dials, sessions)
}

/// Everything the client wrote, across every connection the dialler opened.
fn all_written(sessions: &Mutex<Vec<Recorded>>) -> String {
    sessions.lock().unwrap().iter().map(written).collect()
}

#[tokio::test]
async fn a_second_caller_reuses_the_connection_the_first_gave_back() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5, ALWAYS_CHECK);

    drop(pool.acquire().await.unwrap());
    drop(pool.acquire().await.unwrap());
    drop(pool.acquire().await.unwrap());

    assert_eq!(
        dials.load(AtomicOrdering::SeqCst),
        1,
        "each caller opened its own socket — that is one per folder per pass, which is the \
         behaviour this pool exists to end",
    );
}

#[tokio::test]
async fn the_budget_bounds_how_many_sockets_exist_at_once() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 2, ALWAYS_CHECK);

    let first = pool.acquire().await.unwrap();
    let second = pool.acquire().await.unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);

    // A third caller must wait for a permit, not dial past the budget. If it dials, this
    // resolves and the count rises.
    let waiting = tokio::time::timeout(std::time::Duration::from_millis(50), pool.acquire()).await;
    assert!(
        waiting.is_err(),
        "a third caller was served while the budget was full — the account's socket count is \
         then set by how many folders the host syncs, not by this number",
    );
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);

    drop(first);
    // With a permit free, the same call now succeeds — and reuses, rather than dialling.
    let third = tokio::time::timeout(std::time::Duration::from_millis(500), pool.acquire())
        .await
        .expect("a freed permit must admit the waiting caller")
        .unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
    drop((second, third));
}

#[tokio::test]
async fn a_connection_handed_back_moments_ago_is_reused_without_a_check() {
    // The script answers no NOOP at all: if the pool checked, the check would fail, the
    // connection would be discarded, and a second dial would show up in the count.
    let (dial, dials, sessions) = recording_dialler(false);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    drop(pool.acquire().await.unwrap());
    drop(pool.acquire().await.unwrap());

    assert_eq!(dials.load(AtomicOrdering::SeqCst), 1);
    assert!(
        !all_written(&sessions).contains("NOOP"),
        "a sync pass's back-to-back calls each paid a round trip to prove what the last one \
         just proved",
    );
}

#[tokio::test]
async fn a_checked_acquire_proves_even_a_young_connection_alive() {
    // A retry's first attempt has just shown a young connection can be dead, so the retry
    // must not trust one on age alone.
    let (dial, dials) = dialler(false);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    drop(pool.acquire().await.unwrap());
    let _retry = pool.acquire_checked().await.unwrap();

    assert_eq!(
        dials.load(AtomicOrdering::SeqCst),
        2,
        "the retry reused the connection without asking whether it still answers",
    );
}

#[tokio::test]
async fn a_settled_failure_discards_the_connection_and_a_success_parks_it() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let ok: Result<(), ()> = pool.acquire().await.unwrap().settle(Ok(()));
    assert!(ok.is_ok());
    drop(pool.acquire().await.unwrap());
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 1, "a success parks");

    // A young connection is handed out unchecked, so one whose call failed — a dropped
    // socket, a desynchronised response — must not be there to hand out.
    let failed: Result<(), ()> = pool.acquire().await.unwrap().settle(Err(()));
    assert!(failed.is_err());
    drop(pool.acquire().await.unwrap());
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2, "a failure discards");
}

#[tokio::test]
async fn parked_connections_and_watches_together_stay_within_the_budget() {
    // Two workers each open a socket and park it. A watch dialled beside them would make
    // three sockets on a budget of two, which is the arithmetic that turns five into nine.
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 2, ALWAYS_CHECK);

    let first = pool.acquire().await.unwrap();
    let second = pool.acquire().await.unwrap();
    drop((first, second));
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);

    let _watch = pool.reserve_watch().await.unwrap();
    assert_eq!(
        dials.load(AtomicOrdering::SeqCst),
        2,
        "the watch dialled a socket while a parked one sat unused",
    );
    // The worker left over takes the other parked connection, again without dialling.
    let _worker = pool.acquire().await.unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_connection_that_died_while_resting_is_discarded_rather_than_handed_out() {
    // The dial produces sockets that answer nothing: open, then silent — a NAT idle-timeout, a
    // suspended laptop, a phone that changed radio.
    let (dial, dials) = dialler(false);
    let pool = ImapPool::new(dial, 5, ALWAYS_CHECK);

    drop(pool.acquire().await.unwrap());
    // The parked connection fails its NOOP, so it is dropped and a fresh one dialled. Without
    // the check the caller's own first command fails instead, and it reads like a server fault.
    let _second = pool.acquire().await.unwrap();

    assert_eq!(
        dials.load(AtomicOrdering::SeqCst),
        2,
        "a dead parked connection was handed out as if it were alive",
    );
}

#[tokio::test]
async fn a_watch_reservation_never_takes_the_last_connection() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 3, ALWAYS_CHECK);
    assert_eq!(pool.watch_headroom(), 2);

    let first = pool.reserve_watch().await.unwrap();
    let second = pool.reserve_watch().await.unwrap();
    assert_eq!(pool.watch_headroom(), 0);

    // A budget spent entirely on push is an account that can never sync — a deadlock, not a
    // degradation. So the third reservation is refused, and it says why.
    let refused = pool
        .reserve_watch()
        .await
        .expect_err("the third watch took the last worker");
    // Not a server fault and not a permanent one: it clears when a watch is dropped.
    assert_eq!(refused.class(), FailureClass::InvalidState);
    let message = refused.to_string();
    assert!(
        message.contains("free to sync"),
        "the refusal must name the reason the host can act on: {message}",
    );

    // And a worker can still be served while both watches are held.
    let _worker = tokio::time::timeout(std::time::Duration::from_millis(500), pool.acquire())
        .await
        .expect("the reserved floor must leave a worker able to sync")
        .unwrap();
    drop((first, second));
}

#[tokio::test]
async fn a_dropped_watch_returns_its_slot() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 3, ALWAYS_CHECK);

    let watch = pool.reserve_watch().await.unwrap();
    assert_eq!(pool.watch_headroom(), 1);
    assert!(format!("{:?}", watch.1).contains("WatchLease"));
    // Dropping is the only release path, because a watch task can be aborted rather than
    // stopped — an explicit `release` call would simply not run.
    drop(watch);
    assert_eq!(
        pool.watch_headroom(),
        2,
        "an aborted watch leaked its slot; enough of those and the account cannot sync",
    );
}

#[tokio::test]
async fn a_failed_dial_does_not_leak_the_watch_slot() {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    // A dial that always fails, as it would with no network.
    let dial: Dial<MockStream> = Arc::new(move || {
        counter.fetch_add(1, AtomicOrdering::SeqCst);
        Box::pin(async { Err(ImapError::bad("no network")) })
    });
    let pool = ImapPool::new(dial, 3, ALWAYS_CHECK);

    assert!(pool.reserve_watch().await.is_err());
    assert_eq!(
        pool.watch_headroom(),
        2,
        "the slot was counted before the dial and never returned when it failed — every \
         offline retry would then permanently shrink the budget",
    );
}

#[tokio::test]
async fn invalidating_the_pool_stops_resting_connections_being_reused() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5, ALWAYS_CHECK);

    drop(pool.acquire().await.unwrap());
    // The network went away: the parked socket looks open and is not, and NOOP-ing it would
    // cost a timeout. Dropping the generation is cheaper and certain.
    pool.invalidate();
    let _fresh = pool.acquire().await.unwrap();

    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_guard_in_flight_when_the_pool_is_invalidated_is_not_parked() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5, ALWAYS_CHECK);

    let held = pool.acquire().await.unwrap();
    pool.invalidate();
    // The guard finishes its work normally, then must be discarded rather than returned — it
    // belongs to the generation that was superseded.
    drop(held);
    let _fresh = pool.acquire().await.unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_discarded_connection_is_not_offered_to_the_next_caller() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5, ALWAYS_CHECK);

    // A caller that has left the session in a state the next borrower must not inherit says so,
    // rather than parking it and hoping.
    pool.acquire().await.unwrap().discard();
    let _next = pool.acquire().await.unwrap();

    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_budget_of_zero_is_clamped_rather_than_deadlocking() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 0, ALWAYS_CHECK);
    assert_eq!(pool.max_connections(), 1);
    // A pool that can hold nothing would hang here forever instead of failing visibly.
    let _only = tokio::time::timeout(std::time::Duration::from_millis(500), pool.acquire())
        .await
        .expect("a zero budget must be clamped, not honoured")
        .unwrap();
}
