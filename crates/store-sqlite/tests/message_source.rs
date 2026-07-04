//! The raw message-source cache stores bytes on the filesystem, not in SQLite.

use std::fs;

use engine_core::ids::{AccountId, ProviderKey};
use engine_core::mail::MessageBody;
use engine_core::raw::RawMime;
use engine_store::{ManualClock, MessageBodyStore, MessageSourceCache};
use store_sqlite::SqliteStore;
use tempfile::TempDir;

fn clock() -> ManualClock {
    ManualClock::new("2026-06-26T00:00:00Z".parse().expect("valid instant"))
}

fn account() -> AccountId {
    AccountId::try_from("acct").expect("valid account")
}

fn key(s: &str) -> ProviderKey {
    ProviderKey::new(s).expect("valid provider key")
}

#[tokio::test]
async fn put_then_get_round_trips_through_the_blob_area() {
    let store = SqliteStore::open_in_memory(clock()).expect("open store");
    let raw = RawMime::new(b"From: a@b\r\n\r\nhello body".to_vec());

    assert!(
        store
            .get_message_source(&account(), &key("imap:v1:u1@INBOX"))
            .await
            .expect("get")
            .is_none(),
        "absent before fetch"
    );

    store
        .put_message_source(&account(), &key("imap:v1:u1@INBOX"), raw.clone())
        .await
        .expect("put");

    let got = store
        .get_message_source(&account(), &key("imap:v1:u1@INBOX"))
        .await
        .expect("get")
        .expect("present after put");
    assert_eq!(got.as_bytes(), raw.as_bytes());
}

#[tokio::test]
async fn bytes_land_on_the_filesystem_and_a_missing_blob_reads_as_a_miss() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("store.db");
    let store = SqliteStore::open(&db_path, clock()).expect("open file store");
    let raw = RawMime::new(b"the raw message bytes".to_vec());

    store
        .put_message_source(&account(), &key("imap:v1:u7@INBOX"), raw.clone())
        .await
        .expect("put");

    // The bytes live in `<db>.blobs/sources/`, not the database file.
    let sources = dir.path().join("store.db.blobs").join("sources");
    let blobs: Vec<_> = fs::read_dir(&sources)
        .expect("sources dir exists")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "eml"))
        .collect();
    assert_eq!(blobs.len(), 1, "one blob file written");
    assert_eq!(fs::read(&blobs[0]).expect("read blob"), raw.as_bytes());

    // Removing the blob file makes the entry read as a cache miss (re-fetchable),
    // not a corrupt half-present row.
    fs::remove_file(&blobs[0]).expect("remove blob");
    assert!(
        store
            .get_message_source(&account(), &key("imap:v1:u7@INBOX"))
            .await
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn identical_bytes_dedupe_to_one_blob() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("store.db");
    let store = SqliteStore::open(&db_path, clock()).expect("open file store");
    let raw = RawMime::new(b"shared content across two folders".to_vec());

    // The same message copied into two folders is two distinct keys but identical
    // bytes — content addressing stores one file.
    store
        .put_message_source(&account(), &key("imap:v1:u1@INBOX"), raw.clone())
        .await
        .expect("put inbox");
    store
        .put_message_source(&account(), &key("imap:v1:u1@Archive"), raw.clone())
        .await
        .expect("put archive");

    let sources = dir.path().join("store.db.blobs").join("sources");
    let count = fs::read_dir(&sources)
        .expect("sources dir")
        .filter(|e| {
            e.as_ref()
                .expect("entry")
                .path()
                .extension()
                .is_some_and(|x| x == "eml")
        })
        .count();
    assert_eq!(count, 1, "identical bytes dedupe to one blob");
}

#[tokio::test]
async fn put_overwrites_a_prior_entry_for_the_same_key() {
    let store = SqliteStore::open_in_memory(clock()).expect("open store");
    let first = RawMime::new(b"first version".to_vec());
    let second = RawMime::new(b"second, longer version of the body".to_vec());

    store
        .put_message_source(&account(), &key("imap:v1:u1@INBOX"), first.clone())
        .await
        .expect("put first");
    store
        .put_message_source(&account(), &key("imap:v1:u1@INBOX"), second.clone())
        .await
        .expect("put second");

    let got = store
        .get_message_source(&account(), &key("imap:v1:u1@INBOX"))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.as_bytes(), second.as_bytes());
}

#[tokio::test]
async fn a_corrupted_blob_reads_as_a_miss() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("store.db");
    let store = SqliteStore::open(&db_path, clock()).expect("open file store");
    store
        .put_message_source(
            &account(),
            &key("imap:v1:u1@INBOX"),
            RawMime::new(b"the real bytes".to_vec()),
        )
        .await
        .expect("put");

    // Overwrite the blob with different content — its name no longer matches its
    // hash, so verify-on-read treats it as a miss rather than serving wrong bytes.
    let sources = dir.path().join("store.db.blobs").join("sources");
    let blob = fs::read_dir(&sources)
        .expect("sources dir")
        .map(|e| e.expect("entry").path())
        .find(|p| p.extension().is_some_and(|x| x == "eml"))
        .expect("one blob");
    fs::write(&blob, b"tampered/truncated content").expect("corrupt blob");

    assert!(
        store
            .get_message_source(&account(), &key("imap:v1:u1@INBOX"))
            .await
            .expect("get")
            .is_none()
    );
}

#[tokio::test]
async fn body_text_round_trips_through_sqlite() {
    let store = SqliteStore::open_in_memory(clock()).expect("open store");
    let k = key("imap:v1:u1@INBOX");

    assert!(
        store
            .get_message_body(&account(), &k)
            .await
            .expect("get")
            .is_none(),
        "absent before extraction"
    );

    let body = MessageBody::new(
        Some("plain text".to_owned()),
        Some("<p>html</p>".to_owned()),
    );
    store
        .put_message_body(&account(), &k, &body)
        .await
        .expect("put body");
    let got = store
        .get_message_body(&account(), &k)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got, body);

    // A later extraction with no HTML part overwrites and round-trips html = None.
    let plain_only = MessageBody::new(Some("just plain".to_owned()), None);
    store
        .put_message_body(&account(), &k, &plain_only)
        .await
        .expect("put plain-only");
    assert_eq!(
        store.get_message_body(&account(), &k).await.expect("get"),
        Some(plain_only)
    );
}
