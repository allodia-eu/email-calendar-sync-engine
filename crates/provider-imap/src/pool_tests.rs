//! Offline tests for the bounded connection pool, over scripted mock streams.
//!
//! The properties here are the ones a real server punishes and no unit under test can observe
//! on its own: how many sockets exist at once, whether a resting one is proved alive before
//! reuse, and whether watches can crowd out syncing. Each is asserted by counting *dials*,
//! because the dial is the socket — a pool that quietly opens one more is exactly the bug.

use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use super::*;
use crate::mock::{MockStream, script};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] ready\r\n";
/// Enough `NOOP` answers that a reused connection can be validated several times over. A
/// parked connection is checked on every hand-out, so a script that answers once would make a
/// third acquire look like a dead socket and quietly change what the test measures.
const NOOPS: &str = "a1 OK NOOP done\r\na2 OK NOOP done\r\na3 OK NOOP done\r\na4 OK NOOP done\r\n";

/// A dial factory over mock streams, and the count of how many times it was called.
///
/// `healthy` scripts a connection that answers `NOOP`; `dead` scripts one whose stream is
/// exhausted, which is how a socket that died while resting behaves — it looks open until
/// something is written to it.
fn dialler(healthy: bool) -> (Dial<MockStream>, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    let dial: Dial<MockStream> = Arc::new(move || {
        counter.fetch_add(1, AtomicOrdering::SeqCst);
        let server = if healthy {
            script(&[GREETING, NOOPS])
        } else {
            script(&[GREETING])
        };
        Box::pin(async move {
            let (stream, _recorded) = MockStream::new(server);
            Connection::open(stream).await
        })
    });
    (dial, dials)
}

#[tokio::test]
async fn a_second_caller_reuses_the_connection_the_first_gave_back() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5);

    drop(pool.acquire(None).await.unwrap());
    drop(pool.acquire(None).await.unwrap());
    drop(pool.acquire(None).await.unwrap());

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
    let pool = ImapPool::new(dial, 2);

    let first = pool.acquire(None).await.unwrap();
    let second = pool.acquire(None).await.unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);

    // A third caller must wait for a permit, not dial past the budget. If it dials, this
    // resolves and the count rises.
    let waiting = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        pool.acquire(Some("INBOX")),
    )
    .await;
    assert!(
        waiting.is_err(),
        "a third caller was served while the budget was full — the account's socket count is \
         then set by how many folders the host syncs, not by this number",
    );
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);

    drop(first);
    // With a permit free, the same call now succeeds — and reuses, rather than dialling.
    let third = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        pool.acquire(Some("INBOX")),
    )
    .await
    .expect("a freed permit must admit the waiting caller")
    .unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
    drop((second, third));
}

#[tokio::test]
async fn a_caller_is_given_the_connection_that_already_has_its_mailbox_selected() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 5);

    // Two connections park with different mailboxes selected.
    let mut archive = pool.acquire(None).await.unwrap();
    archive.note_selected("Archive");
    let mut inbox = pool.acquire(None).await.unwrap();
    inbox.note_selected("INBOX");
    drop(archive);
    drop(inbox);

    // SELECT costs a round trip and discards the CONDSTORE context, so wanting INBOX must not
    // hand back the connection sitting on Archive just because it was parked first.
    let reused = pool.acquire(Some("INBOX")).await.unwrap();
    assert_eq!(reused.selected(), Some("INBOX"));
}

#[tokio::test]
async fn a_connection_that_died_while_resting_is_discarded_rather_than_handed_out() {
    // The dial produces sockets that answer nothing: open, then silent — a NAT idle-timeout, a
    // suspended laptop, a phone that changed radio.
    let (dial, dials) = dialler(false);
    let pool = ImapPool::new(dial, 5);

    drop(pool.acquire(None).await.unwrap());
    // The parked connection fails its NOOP, so it is dropped and a fresh one dialled. Without
    // the check the caller's own first command fails instead, and it reads like a server fault.
    let _second = pool.acquire(None).await.unwrap();

    assert_eq!(
        dials.load(AtomicOrdering::SeqCst),
        2,
        "a dead parked connection was handed out as if it were alive",
    );
}

#[tokio::test]
async fn a_watch_reservation_never_takes_the_last_connection() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 3);
    assert_eq!(pool.watch_headroom(), 2);

    let first = pool.reserve_watch().await.unwrap();
    let second = pool.reserve_watch().await.unwrap();
    assert_eq!(pool.watch_headroom(), 0);

    // A budget spent entirely on push is an account that can never sync — a deadlock, not a
    // degradation. So the third reservation is refused, and it says why.
    let refused = pool.reserve_watch().await;
    let message = refused
        .expect_err("the third watch took the last worker")
        .to_string();
    assert!(
        message.contains("free to sync"),
        "the refusal must name the reason the host can act on: {message}",
    );

    // And a worker can still be served while both watches are held.
    let _worker = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        pool.acquire(Some("INBOX")),
    )
    .await
    .expect("the reserved floor must leave a worker able to sync")
    .unwrap();
    drop((first, second));
}

#[tokio::test]
async fn a_dropped_watch_returns_its_slot() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 3);

    let watch = pool.reserve_watch().await.unwrap();
    assert_eq!(pool.watch_headroom(), 1);
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
    let pool = ImapPool::new(dial, 3);

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
    let pool = ImapPool::new(dial, 5);

    drop(pool.acquire(None).await.unwrap());
    // The network went away: the parked socket looks open and is not, and NOOP-ing it would
    // cost a timeout. Dropping the generation is cheaper and certain.
    pool.invalidate();
    let _fresh = pool.acquire(None).await.unwrap();

    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_guard_in_flight_when_the_pool_is_invalidated_is_not_parked() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5);

    let held = pool.acquire(None).await.unwrap();
    pool.invalidate();
    // The guard finishes its work normally, then must be discarded rather than returned — it
    // belongs to the generation that was superseded.
    drop(held);
    let _fresh = pool.acquire(None).await.unwrap();
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_discarded_connection_is_not_offered_to_the_next_caller() {
    let (dial, dials) = dialler(true);
    let pool = ImapPool::new(dial, 5);

    // A caller that has left the session in a state the next borrower must not inherit says so,
    // rather than parking it and hoping.
    pool.acquire(None).await.unwrap().discard();
    let _next = pool.acquire(None).await.unwrap();

    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_budget_of_zero_is_clamped_rather_than_deadlocking() {
    let (dial, _dials) = dialler(true);
    let pool = ImapPool::new(dial, 0);
    assert_eq!(pool.max_connections(), 1);
    // A pool that can hold nothing would hang here forever instead of failing visibly.
    let _only = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        pool.acquire(Some("INBOX")),
    )
    .await
    .expect("a zero budget must be clamped, not honoured")
    .unwrap();
}
