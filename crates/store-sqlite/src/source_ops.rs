//! The on-demand raw message-source cache.
//!
//! Implements [`MessageSourceCache`] for [`SqliteStore`]: the (potentially large,
//! attachment-bearing) bytes go to the content-addressed filesystem blob area
//! (`blob.rs`), and only the metadata row — the blob's content hash, byte length,
//! and fetch instant — lands in SQLite (`schema.rs` `message_source`). The cache is
//! lease-free and idempotent, so it never contends with a sync holding the message's
//! scope (`store-and-sync.md`).

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::raw::RawMime;
use engine_store::{Clock, MessageSourceCache, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::SqliteStore;
use crate::blob;
use crate::convert::{backend, instant_to_text};

#[async_trait]
impl<C: Clock> MessageSourceCache for SqliteStore<C> {
    async fn put_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        source: &RawMime,
    ) -> Result<()> {
        let fetched_at = instant_to_text(self.clock.now());
        let root = self.blobs.root().to_path_buf();
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        let bytes = source.as_bytes().to_vec();
        self.call(move |conn| {
            let byte_len = i64::try_from(bytes.len()).map_err(backend)?;
            let hash = blob::write_source(&root, &bytes)?;
            upsert_source(conn, &account, &key, &hash, byte_len, &fetched_at)
        })
        .await
    }

    async fn get_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<Option<RawMime>> {
        let root = self.blobs.root().to_path_buf();
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        self.call(move |conn| {
            let Some(hash) = select_hash(conn, &account, &key)? else {
                return Ok(None);
            };
            // A missing blob (evicted/externally removed) reads as a cache miss, so
            // the caller re-fetches rather than seeing a half-present entry.
            Ok(blob::read_source(&root, &hash)?.map(RawMime::new))
        })
        .await
    }
}

/// Upserts the metadata row mapping `(account, key)` to its blob's content hash.
fn upsert_source(
    conn: &Connection,
    account: &str,
    key: &str,
    hash: &str,
    byte_len: i64,
    fetched_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO message_source
             (account, provider_key, content_hash, byte_len, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account, provider_key) DO UPDATE SET
             content_hash = excluded.content_hash,
             byte_len     = excluded.byte_len,
             fetched_at   = excluded.fetched_at",
        (account, key, hash, byte_len, fetched_at),
    )
    .map_err(backend)?;
    Ok(())
}

/// Reads the blob content hash recorded for `(account, key)`, if any.
fn select_hash(conn: &Connection, account: &str, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT content_hash FROM message_source
         WHERE account = ?1 AND provider_key = ?2",
        (account, key),
        |row| row.get(0),
    )
    .optional()
    .map_err(backend)
}
