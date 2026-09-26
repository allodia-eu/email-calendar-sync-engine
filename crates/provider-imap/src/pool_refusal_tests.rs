//! Offline tests for what the pool does when a dial fails beside connections that work, and
//! for which connection it hands a caller that names a mailbox.
//!
//! The first matters because five is our number, not the server's: a server that allows this
//! account fewer sessions answers the extra `LOGIN` with a refusal, and failing the caller for
//! it would turn a working account into a failed sync.

use std::{
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    time::Duration,
};

use super::*;
use crate::mock::{MockStream, script};

const GREETING: &str = "* OK [CAPABILITY IMAP4rev1] ready\r\n";
/// A rest no test outlives, so a parked connection is reused without a `NOOP`.
const NEVER_CHECK: Duration = Duration::from_hours(1);

/// A dial that opens `allowed` connections and then fails every later one with `refusal`, as a
/// server does once this account holds as many sessions as it will give it.
fn limited_dialler(
    allowed: usize,
    refusal: fn() -> ImapError,
) -> (Dial<MockStream>, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    let dial: Dial<MockStream> = Arc::new(move || {
        let attempt = counter.fetch_add(1, AtomicOrdering::SeqCst);
        if attempt >= allowed {
            return Box::pin(async move { Err(refusal()) });
        }
        let (stream, _recorded) = MockStream::new(script(&[GREETING]));
        Box::pin(async move { Connection::open(stream).await })
    });
    (dial, dials)
}

fn login_refused() -> ImapError {
    ImapError::auth("[AUTHENTICATIONFAILED] Authentication failed.")
}

fn unreachable() -> ImapError {
    ImapError::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "reset",
    ))
}

const SOON: Duration = Duration::from_millis(500);

#[tokio::test]
async fn a_refused_dial_beside_a_working_connection_waits_for_it() {
    let (dial, dials) = limited_dialler(1, login_refused);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let held = pool.acquire().await.unwrap();
    let waiting = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.acquire().await.map(drop) }
    });
    // The second caller's dial is refused. It must not fail: a connection is coming back.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "the refusal failed the caller");

    drop(held);
    tokio::time::timeout(SOON, waiting)
        .await
        .expect("the returned connection must admit the waiting caller")
        .unwrap()
        .expect("the waiting caller is served the connection that came back");
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn a_refusal_lowers_the_ceiling_so_the_pool_stops_asking() {
    let (dial, dials) = limited_dialler(2, login_refused);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let first = pool.acquire().await.unwrap();
    let second = pool.acquire().await.unwrap();
    let third = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.acquire().await.map(drop) }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        pool.ceiling(),
        4,
        "the refused session is still counted as available"
    );
    assert_eq!(pool.worker_capacity(), 4);

    drop(first);
    tokio::time::timeout(SOON, third)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(second);
    // Two connections are parked and the ceiling is four: the next two callers reuse them, and
    // no further dial is attempted until they are both out.
    let reused = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    assert_eq!(dials.load(AtomicOrdering::SeqCst), 3);
    drop(reused);
}

#[tokio::test]
async fn a_network_change_restores_a_lowered_ceiling() {
    let (dial, _dials) = limited_dialler(1, login_refused);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let held = pool.acquire().await.unwrap();
    let waiting = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.acquire().await.map(drop) }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    tokio::time::timeout(SOON, waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(pool.ceiling(), 4);

    // A new network is a new address, and a per-address limit starts again.
    pool.invalidate();
    assert_eq!(pool.ceiling(), 5);
    assert_eq!(pool.watch_headroom(), 4);
}

#[tokio::test]
async fn an_unanswered_dial_waits_without_lowering_the_ceiling() {
    // A reset says nothing about the server's limit: it waits for the working connection, but
    // the budget stays as it was.
    let (dial, _dials) = limited_dialler(1, unreachable);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let held = pool.acquire().await.unwrap();
    let waiting = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move { pool.acquire().await.map(drop) }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(held);
    tokio::time::timeout(SOON, waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(pool.ceiling(), 5);
}

#[tokio::test]
async fn a_refused_dial_with_nothing_to_wait_for_fails() {
    let (dial, _dials) = limited_dialler(0, login_refused);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let Err(err) = tokio::time::timeout(SOON, pool.acquire())
        .await
        .expect("a caller with no connection coming back must not wait")
    else {
        panic!("the dial was refused, so there is no connection to hand out");
    };
    assert!(matches!(err, ImapError::Auth(_)), "{err}");
    assert_eq!(
        pool.ceiling(),
        5,
        "a refusal with nothing held is not a limit"
    );
}

#[tokio::test]
async fn watches_leave_the_rest_of_the_ceiling_to_workers() {
    let (dial, _dials) = limited_dialler(5, login_refused);
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);
    assert_eq!(pool.worker_capacity(), 5);

    let watch = pool.reserve_watch().await.unwrap();
    assert_eq!(pool.worker_capacity(), 4);
    drop(watch);
    assert_eq!(pool.worker_capacity(), 5);
}

#[tokio::test]
async fn a_caller_naming_a_mailbox_gets_the_connection_that_has_it_open() {
    // Two parked connections, the older one with INBOX open. A caller reading INBOX is handed
    // that one, so its read needs no `EXAMINE`; anyone else gets the most recently parked.
    let dial: Dial<MockStream> = Arc::new(|| {
        let (stream, _recorded) = MockStream::new(script(&[
            GREETING,
            "* OK [UIDVALIDITY 7] v\r\na1 OK [READ-ONLY] done\r\n",
        ]));
        Box::pin(async move { Connection::open(stream).await })
    });
    let pool = ImapPool::new(dial, 5, NEVER_CHECK);

    let mut inbox = pool.acquire().await.unwrap();
    inbox.examine("INBOX").await.unwrap();
    let other = pool.acquire().await.unwrap();
    drop(inbox);
    drop(other);

    let handed = pool.acquire_for("INBOX").await.unwrap();
    assert_eq!(handed.open_validity("INBOX"), Some(7));
    let next = pool.acquire().await.unwrap();
    assert_eq!(next.open_validity("INBOX"), None);
}
