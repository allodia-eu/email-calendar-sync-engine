//! The SQLite store must satisfy the full store contract, unchanged.
//!
//! This is the same `engine_store::contract::run_all` suite the in-memory
//! reference store passes; every backend must pass it identically. Each case gets
//! a fresh database, so the cases stay isolated.
//!
//! The suite runs **twice**, because the two constructions are not the same store.
//! An in-memory database is a single connection; a file database is a writer plus a
//! pool of `query_only` readers, and only there does a write routed to a reader
//! fail. Running `:memory:` alone would leave every routing decision unexercised.

use std::sync::atomic::{AtomicUsize, Ordering};

use engine_store::{ManualClock, contract};
use store_sqlite::SqliteStore;

fn clock() -> ManualClock {
    ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"))
}

#[tokio::test]
async fn sqlite_store_satisfies_contract_in_memory() {
    contract::run_all(|| {
        let clock = clock();
        let store = SqliteStore::open_in_memory(clock.clone()).expect("open in-memory store");
        (store, clock)
    })
    .await;
    contract::run_contacts(|| {
        let clock = clock();
        let store = SqliteStore::open_in_memory(clock.clone()).expect("open in-memory store");
        (store, clock)
    })
    .await;
}

#[tokio::test]
async fn sqlite_store_satisfies_contract_on_disk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let next = AtomicUsize::new(0);
    // A distinct file per case: the suite's isolation guarantee is per store, and a
    // reused path would carry the previous case's rows into the next one.
    let make = || {
        let clock = clock();
        let path = dir.path().join(format!(
            "case-{}.sqlite",
            next.fetch_add(1, Ordering::Relaxed)
        ));
        let store = SqliteStore::open(&path, clock.clone()).expect("open file store");
        (store, clock)
    };
    contract::run_all(&make).await;
    contract::run_contacts(&make).await;
}
