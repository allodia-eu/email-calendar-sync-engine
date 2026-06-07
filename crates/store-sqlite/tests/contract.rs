//! The SQLite store must satisfy the full store contract, unchanged.
//!
//! This is the same `engine_store::contract::run_all` suite the in-memory
//! reference store passes; every backend must pass it identically. Each case gets
//! a fresh `:memory:` database (one connection = one database), so the cases stay
//! isolated.

use engine_store::ManualClock;
use engine_store::contract;
use store_sqlite::SqliteStore;

#[tokio::test]
async fn sqlite_store_satisfies_contract() {
    contract::run_all(|| {
        let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
        let store = SqliteStore::open_in_memory(clock.clone()).expect("open in-memory store");
        (store, clock)
    })
    .await;
}
