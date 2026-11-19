//! The outbox across a restart: a file store written by one "process", dropped as a dead
//! process drops it, and opened again by the next. Also the store an older build left, and two
//! stores sharing one file as an app and its background task do.

use core::time::Duration;
use std::{path::Path, sync::Arc};

use engine_core::{
    ids::AccountId,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, ResourceKey},
};
use engine_store::{
    ClaimRejection, LeaseRequest, ManualClock, OpLease, PendingOpClaim, PendingOpState, Store,
    StoreRead, WorkerId,
};
use rusqlite::Connection;
use serde_json::json;

use crate::SqliteStore;

fn clock() -> ManualClock {
    ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap())
}

fn account() -> AccountId {
    AccountId::try_from("acct-restart").unwrap()
}

fn op(kind: PendingOpKind, name: &str) -> PendingOp {
    PendingOp::new(
        IdempotencyKey::new(name).unwrap(),
        kind,
        ResourceKey::new(format!("res:{name}")).unwrap(),
        json!({ "name": name }),
    )
}

fn request(worker: &str) -> LeaseRequest {
    LeaseRequest::new(WorkerId::new(worker), Duration::from_mins(5))
}

async fn claim(store: &SqliteStore<ManualClock>, id: PendingOpId) -> OpLease {
    match store
        .claim_pending_op(account(), id, request("first-process"))
        .await
        .unwrap()
    {
        PendingOpClaim::Leased(leased) => leased.lease,
        PendingOpClaim::Refused(why) => panic!("{id:?} refused: {why:?}"),
    }
}

/// One process: enqueues two sends and starts both, handing one over, then dies.
async fn die_mid_send(path: &Path, clock: &ManualClock) -> (PendingOpId, PendingOpId) {
    let store = SqliteStore::open(path, clock.clone()).unwrap();
    let connecting = store
        .enqueue_pending_op(account(), op(PendingOpKind::MailSubmit, "connecting"))
        .await
        .unwrap();
    let handed = store
        .enqueue_pending_op(account(), op(PendingOpKind::MailSubmit, "handed"))
        .await
        .unwrap();
    claim(&store, connecting).await;
    let lease = claim(&store, handed).await;
    store.record_hand_over(&lease).await.unwrap();
    drop(store);
    (connecting, handed)
}

async fn state(store: &SqliteStore<ManualClock>, id: PendingOpId) -> PendingOpState {
    store
        .list_pending_ops(account())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .map(|row| row.state)
        .expect("listed")
}

#[tokio::test]
async fn the_start_up_call_recovers_what_the_last_process_left_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let clock = clock();
    let (connecting, handed) = die_mid_send(&path, &clock).await;

    // The next process, a moment later: both leases are still live on the clock.
    let store = SqliteStore::open(&path, clock.clone()).unwrap();
    assert_eq!(state(&store, connecting).await, PendingOpState::InFlight);
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 2);

    assert_eq!(state(&store, connecting).await, PendingOpState::Pending);
    assert_eq!(
        state(&store, handed).await,
        PendingOpState::NeedsConfirmation
    );
    assert!(matches!(
        store
            .claim_pending_op(account(), connecting, request("next-process"))
            .await
            .unwrap(),
        PendingOpClaim::Leased(_)
    ));
    assert_eq!(
        store
            .claim_pending_op(account(), handed, request("next-process"))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );
}

#[tokio::test]
async fn a_lapsed_lease_is_recovered_after_a_restart_without_the_start_up_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let clock = clock();
    let (connecting, handed) = die_mid_send(&path, &clock).await;

    clock.advance(Duration::from_mins(6));
    let store = SqliteStore::open(&path, clock.clone()).unwrap();
    // The reads already say what the recovery will do; the claim does it.
    assert_eq!(state(&store, connecting).await, PendingOpState::Pending);
    assert_eq!(
        state(&store, handed).await,
        PendingOpState::NeedsConfirmation
    );
    let claimed = store
        .claim_pending_ops(account(), request("next-process"), 10)
        .await
        .unwrap();
    assert_eq!(
        claimed.iter().map(|op| op.id).collect::<Vec<_>>(),
        vec![connecting]
    );
    drop(store);

    // And it was durable: a third process sees the same.
    let store = SqliteStore::open(&path, clock.clone()).unwrap();
    assert_eq!(
        state(&store, handed).await,
        PendingOpState::NeedsConfirmation
    );
    assert_eq!(state(&store, connecting).await, PendingOpState::InFlight);
}

/// A send in flight under a build that recorded no hand-over cannot prove it stopped short of
/// the server, so after the upgrade it awaits confirmation; anything else in flight is retried
/// as before, and what nobody held is untouched.
#[tokio::test]
async fn an_op_in_flight_before_the_upgrade_recovers_safely() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        crate::migrations::migrate_to(&mut conn, 15).unwrap();
        for (key, kind, state) in [
            ("send-in-flight", "MailSubmit", "InFlight"),
            ("edit-in-flight", "MailEdit", "InFlight"),
            ("send-queued", "MailSubmit", "Pending"),
        ] {
            conn.execute(
                "INSERT INTO pending_op
                     (account, kind, idempotency_key, resource_key, depends_on, payload, state,
                      token, lease_expiry, attempts)
                 VALUES ('acct-restart', ?1, ?2, ?2, '[]', '{}', ?3, 4,
                         '2026-01-01T00:05:00Z', 0)",
                (kind, key, state),
            )
            .unwrap();
        }
    }

    let store = SqliteStore::open(&path, clock()).unwrap();
    assert_eq!(store.schema_status().await.unwrap().migrated_from, Some(15));
    assert_eq!(store.recover_interrupted_ops().await.unwrap(), 2);
    let rows = store.list_pending_ops(account()).await.unwrap();
    let by_key = |key: &str| {
        rows.iter()
            .find(|row| row.idempotency_key.as_str() == key)
            .expect("listed")
    };
    assert_eq!(
        by_key("send-in-flight").state,
        PendingOpState::NeedsConfirmation
    );
    assert_eq!(by_key("edit-in-flight").state, PendingOpState::Pending);
    assert_eq!(by_key("send-queued").state, PendingOpState::Pending);
    assert_eq!(by_key("send-queued").attempts, 0);
}

/// Two stores on one file, as an app and its background task hold it: however the claims
/// interleave, exactly one of them leases the op, and after its lease lapses exactly one
/// takes it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_stores_on_one_file_lease_an_op_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.db");
    let clock = clock();
    let first = Arc::new(SqliteStore::open(&path, clock.clone()).unwrap());
    let second = Arc::new(SqliteStore::open(&path, clock.clone()).unwrap());
    let id = first
        .enqueue_pending_op(account(), op(PendingOpKind::MailSubmit, "raced"))
        .await
        .unwrap();

    for round in 0..2 {
        let mut racers = Vec::new();
        for n in 0..16 {
            let store = Arc::clone(if n % 2 == 0 { &first } else { &second });
            racers.push(tokio::spawn(async move {
                let worker = format!("worker-{n}");
                store
                    .claim_pending_op(account(), id, request(&worker))
                    .await
                    .unwrap()
            }));
        }
        let mut leased = 0;
        for racer in racers {
            match racer.await.unwrap() {
                PendingOpClaim::Leased(_) => leased += 1,
                PendingOpClaim::Refused(ClaimRejection::Busy) => {}
                PendingOpClaim::Refused(other) => panic!("round {round}: {other:?}"),
            }
        }
        assert_eq!(leased, 1, "round {round}");
        // The winner dies without a hand-over; once its lease lapses the op is due again.
        clock.advance(Duration::from_mins(6));
    }
}

#[test]
fn a_durable_write_is_synced_and_leaves_the_writer_as_it_found_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("store.db")).unwrap();
    crate::pool::tune(&conn, true).unwrap();
    let level = |conn: &Connection| -> i64 {
        conn.pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap()
    };
    assert_eq!(level(&conn), 1, "a file store runs NORMAL");
    let inside = super::durably(&mut conn, |conn| Ok(level(conn))).unwrap();
    assert_eq!(inside, 2, "FULL: the commit is on disk before it returns");
    assert_eq!(level(&conn), 1);
    let failed: engine_store::Result<()> = super::durably(&mut conn, |_| {
        Err(engine_store::StoreError::Backend("refused".to_owned()))
    });
    assert!(failed.is_err());
    assert_eq!(level(&conn), 1, "restored after a failed write too");
}
