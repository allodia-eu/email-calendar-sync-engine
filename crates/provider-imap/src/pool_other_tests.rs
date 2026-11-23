//! A connection to another of the account's services spends from the same budget as IMAP's, and
//! never takes the account's sockets over the ceiling.

use std::{sync::Arc, time::Duration};

use super::super::{Dial, ImapPool};
use crate::{mock::MockStream, transport::Connection};

fn dial() -> Dial<MockStream> {
    Arc::new(|| {
        let (stream, _) = MockStream::new(b"* OK ready\r\n".to_vec());
        Box::pin(async move { Connection::open(stream).await })
    })
}

fn parked(pool: &ImapPool<MockStream>) -> usize {
    pool.parked.lock().unwrap().len()
}

#[tokio::test]
async fn a_slot_for_another_service_closes_a_parked_socket_only_when_the_shelf_is_full() {
    let pool = ImapPool::new(dial(), 2, Duration::from_hours(1));
    // Two connections dialled at once, then both parked: two sockets, no permit held.
    let (first, second) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    drop((first, second));
    assert_eq!(parked(&pool), 2);

    // One slot for ManageSieve: two parked and one in use would be three sockets of two.
    let slot = pool.reserve_other().await;
    assert_eq!(parked(&pool), 1, "the longest-parked socket is closed");
    assert_eq!(pool.permits.available_permits(), 1);
    drop(slot);
    assert_eq!(pool.permits.available_permits(), 2, "the slot comes back");

    // With room on the shelf, nothing parked is closed.
    let _slot = pool.reserve_other().await;
    assert_eq!(parked(&pool), 1);
}

#[tokio::test(start_paused = true)]
async fn a_slot_waits_while_every_permit_is_spent() {
    let pool = ImapPool::new(dial(), 1, Duration::from_hours(1));
    let worker = pool.acquire().await.unwrap();
    let budget: Arc<dyn super::Budget> = pool.clone();
    let waiting = tokio::time::timeout(Duration::from_secs(1), budget.reserve()).await;
    assert!(waiting.is_err(), "no slot while the only permit is held");
    drop(worker);
    let _slot = budget.reserve().await;
    assert_eq!(
        parked(&pool),
        0,
        "one socket, one slot: the parked one gives way"
    );
}
