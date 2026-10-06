//! Offline tests for borrowing a spare worker: it never waits and never dials past the budget.

use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use super::*;
use crate::{
    error::ImapError,
    mock::{MockStream, script},
    pool::Dial,
    transport::Connection,
};

const NEVER_CHECK: Duration = Duration::from_hours(1);

/// Dials a scripted healthy connection, or the refusal `refuse` names, counting each dial.
fn dialler(refuse: bool) -> (Dial<MockStream>, Arc<AtomicUsize>) {
    let dials = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dials);
    let dial: Dial<MockStream> = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if refuse {
                return Err(ImapError::RateLimited("LOGIN Rate limit hit.".into()));
            }
            let (stream, _) = MockStream::new(script(&["* OK ready\r\n"]));
            Connection::open(stream).await
        })
    });
    (dial, dials)
}

#[tokio::test]
async fn a_spare_worker_is_lent_while_the_budget_has_one() {
    let (dial, dials) = dialler(false);
    let pool = ImapPool::new(dial, 2, NEVER_CHECK);
    let _pass = pool.acquire().await.unwrap();

    let spare = pool.try_acquire_for("INBOX").await;

    assert!(spare.is_some());
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_spent_budget_lends_nothing_and_dials_nothing() {
    let (dial, dials) = dialler(false);
    let pool = ImapPool::new(dial, 2, NEVER_CHECK);
    let _first = pool.acquire().await.unwrap();
    let _second = pool.acquire().await.unwrap();

    // Answers at once rather than waiting for a worker to come back.
    let spare = tokio::time::timeout(Duration::from_secs(5), pool.try_acquire_for("INBOX"))
        .await
        .expect("a spare borrow never waits");

    assert!(spare.is_none());
    assert_eq!(dials.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_refused_spare_dial_lowers_the_ceiling() {
    let (dial, _) = dialler(true);
    let pool = ImapPool::new(dial, 3, NEVER_CHECK);

    assert!(pool.try_acquire_for("INBOX").await.is_none());

    assert_eq!(pool.ceiling(), 2);
}
