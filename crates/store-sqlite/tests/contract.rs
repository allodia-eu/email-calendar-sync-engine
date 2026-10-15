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

use engine_core::time::UtcDateTime;
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
    every_stored_instant_is_fixed_width(dir.path());
}

/// Every value the suite left that reads as an instant is in the one form that sorts as text
/// ([`UtcDateTime::to_sortable_string`]): SQL orders and filters these columns as text, so a writer
/// that formats an instant its own way breaks that silently, and only within a second.
fn every_stored_instant_is_fixed_width(dir: &std::path::Path) {
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("the case files") {
        let path = entry.expect("a case file").path();
        if path.extension().is_none_or(|ext| ext != "sqlite") {
            continue;
        }
        let conn = rusqlite::Connection::open(&path).expect("open a case file");
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for table in tables {
            let columns: Vec<(String, String)> = conn
                .prepare(&format!(
                    "SELECT name, type FROM pragma_table_info('{table}')"
                ))
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for (column, kind) in columns.iter().filter(|(_, kind)| kind == "TEXT") {
                let _ = kind;
                let values: Vec<String> = conn
                    .prepare(&format!(
                        "SELECT {column} FROM \"{table}\" WHERE typeof({column}) = 'text'"
                    ))
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                for value in values {
                    if value.parse::<UtcDateTime>().is_ok() {
                        assert_eq!(
                            value,
                            value.parse::<UtcDateTime>().unwrap().to_sortable_string(),
                            "{table}.{column} holds an instant in a form that does not sort as text"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(
        checked > 0,
        "the suite stored no instant, so this proves nothing"
    );
}
