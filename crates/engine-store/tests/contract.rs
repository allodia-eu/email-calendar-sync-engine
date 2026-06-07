//! The in-memory reference store must satisfy the full store contract.

use engine_store::ManualClock;
use engine_store::contract;
use engine_store::mem::MemStore;

#[tokio::test]
async fn in_memory_store_satisfies_contract() {
    contract::run_all(|| {
        let clock = ManualClock::new("2026-01-01T00:00:00Z".parse().expect("valid instant"));
        let store = MemStore::new(clock.clone());
        (store, clock)
    })
    .await;
}
