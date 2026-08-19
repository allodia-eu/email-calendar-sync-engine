//! Unit tests for the blob sweep: it removes exactly the files no row names, keeps a
//! blob a second row still shares, spares one young enough to be mid-write, and reports
//! the bytes it freed.

use std::{fs, path::Path, time::Duration};

use engine_core::{ids::ProviderKey, raw::RawMime};
use engine_store::{ManualClock, MessageSourceCache, SweepReport};

use super::*;

fn store() -> SqliteStore<ManualClock> {
    SqliteStore::open_in_memory(ManualClock::new("2026-01-01T00:00:00Z".parse().unwrap())).unwrap()
}

fn account(id: &str) -> engine_core::ids::AccountId {
    engine_core::ids::AccountId::try_from(id).unwrap()
}

fn key(id: &str) -> ProviderKey {
    ProviderKey::new(id).unwrap()
}

/// The `sources` blob files present, by name.
fn blob_names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join("sources"))
        .map(|entries| {
            entries
                .filter_map(|entry| Some(entry.ok()?.file_name().to_str()?.to_owned()))
                .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "eml"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Backdates every blob file past the sweep's grace period, so a test does not sleep.
fn age_blobs(root: &Path) {
    let stale = SystemTime::now() - Duration::from_hours(1);
    for (namespace, _) in blob::NAMESPACES {
        let Ok(entries) = fs::read_dir(root.join(namespace)) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = fs::OpenOptions::new()
                .write(true)
                .open(entry.path())
                .unwrap();
            file.set_modified(stale).unwrap();
        }
    }
}

#[tokio::test]
async fn removes_the_blob_no_row_names_and_keeps_the_one_that_is_named() {
    let store = store();
    store
        .put_message_source(
            &account("a"),
            &key("kept"),
            RawMime::new(b"kept bytes".to_vec()),
        )
        .await
        .unwrap();
    store
        .put_message_source(
            &account("a"),
            &key("gone"),
            RawMime::new(b"gone bytes".to_vec()),
        )
        .await
        .unwrap();
    let root = store.blobs.root().to_path_buf();
    assert_eq!(blob_names(&root).len(), 2);

    // Drop one row the way a tombstone does, leaving its blob unreferenced.
    store
        .call(|conn| {
            conn.execute("DELETE FROM message_source WHERE provider_key = 'gone'", [])
                .unwrap();
        })
        .await;
    age_blobs(&root);

    let report = store.sweep_unreferenced_blobs().await.unwrap();

    assert_eq!(report.blobs_removed, 1);
    assert_eq!(report.bytes_reclaimed, "gone bytes".len() as u64);
    assert_eq!(blob_names(&root).len(), 1);
    assert!(
        store
            .get_message_source(&account("a"), &key("kept"))
            .await
            .unwrap()
            .is_some(),
        "the still-referenced source was swept"
    );
}

#[tokio::test]
async fn a_blob_two_rows_share_survives_losing_one_of_them() {
    // Content addressing means one file backs both copies, so the sweep must ask whether
    // *any* row names the hash — not whether the row it was reached through still exists.
    let store = store();
    for (owner, holder) in [("a", "first"), ("b", "second")] {
        store
            .put_message_source(
                &account(owner),
                &key(holder),
                RawMime::new(b"same".to_vec()),
            )
            .await
            .unwrap();
    }
    let root = store.blobs.root().to_path_buf();
    assert_eq!(blob_names(&root).len(), 1, "identical bytes must dedupe");

    store
        .call(|conn| {
            conn.execute(
                "DELETE FROM message_source WHERE provider_key = 'first'",
                [],
            )
            .unwrap();
        })
        .await;
    age_blobs(&root);

    assert_eq!(
        store.sweep_unreferenced_blobs().await.unwrap(),
        SweepReport::default()
    );
    assert_eq!(blob_names(&root).len(), 1);
}

#[tokio::test]
async fn a_blob_young_enough_to_be_mid_write_is_spared() {
    // The file is written before the row that names it, so an unreferenced *new* blob is
    // indistinguishable from one whose row has not landed yet.
    let store = store();
    store
        .put_message_source(
            &account("a"),
            &key("fresh"),
            RawMime::new(b"fresh".to_vec()),
        )
        .await
        .unwrap();
    let root = store.blobs.root().to_path_buf();
    store
        .call(|conn| {
            conn.execute("DELETE FROM message_source", []).unwrap();
        })
        .await;

    assert_eq!(
        store.sweep_unreferenced_blobs().await.unwrap(),
        SweepReport::default(),
        "a blob written moments ago was swept"
    );
    assert_eq!(blob_names(&root).len(), 1);
}

#[tokio::test]
async fn an_empty_blob_area_sweeps_nothing() {
    assert_eq!(
        store().sweep_unreferenced_blobs().await.unwrap(),
        SweepReport::default()
    );
}
