//! The on-demand message-content caches: raw *bytes* on the filesystem, body *text*
//! in SQLite (`store-and-sync.md`).
//!
//! Both implement lease-free, idempotent caches for [`SqliteStore`]. The raw bytes go
//! to the content-addressed filesystem blob area (`blob.rs`) — its read/write runs on
//! a blocking thread **without** the connection lock ([`SqliteStore::block`]), so a
//! multi-megabyte blob never serializes the store; only the small metadata SQL takes
//! the lock. The body text (the reading view + the search source) lives in the
//! `message_body` table, whose `message_body_fts` index a trigger maintains.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ProviderKey},
    mail::MessageBody,
    raw::RawMime,
};
use engine_store::{Clock, MailListRow, MessageBodyStore, MessageSourceCache, Result};
use rusqlite::Connection;

use crate::{SqliteStore, blob, convert::instant_to_text, mail_ops, sql};

#[async_trait]
impl<C: Clock> MessageSourceCache for SqliteStore<C> {
    async fn put_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        source: RawMime,
    ) -> Result<()> {
        // Heavy work — hashing + the blob file write — runs off the connection lock.
        let root = self.blobs.root().to_path_buf();
        let bytes = source.into_bytes();
        let hash = Self::block(move || blob::write_source(&root, &bytes)).await?;

        // Only the tiny metadata upsert takes the connection.
        let fetched_at = instant_to_text(self.clock.now());
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        self.call(move |conn| upsert_source(conn, &account, &key, &hash, &fetched_at))
            .await
    }

    async fn get_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<Option<RawMime>> {
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        let Some(hash) = self
            .read(move |conn| select_hash(conn, &account, &key))
            .await?
        else {
            return Ok(None);
        };
        // The blob read (and its content-hash verification) runs off the lock; a
        // missing/corrupt blob reads as a miss so the caller re-fetches.
        let root = self.blobs.root().to_path_buf();
        Ok(Self::block(move || blob::read_source(&root, &hash))
            .await?
            .map(RawMime::new))
    }
}

#[async_trait]
impl<C: Clock> MessageBodyStore for SqliteStore<C> {
    async fn put_message_body(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        body: &MessageBody,
    ) -> Result<()> {
        let fetched_at = instant_to_text(self.clock.now());
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        let plain = body.plain().unwrap_or_default().to_owned();
        let html = body.html().map(str::to_owned);
        self.call(move |conn| {
            upsert_body(conn, &account, &key, &plain, html.as_deref(), &fetched_at)
        })
        .await
    }

    async fn set_mail_preview(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        preview: &str,
    ) -> Result<()> {
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        let preview = preview.to_owned();
        self.call(move |conn| {
            // `preview IS NULL` is the whole rule: a provider that sent its own snippet keeps
            // it, and a message already filled costs a matched-nothing UPDATE rather than a
            // rewrite. Keyed by account rather than scope because a body is fetched without
            // one — the same message in two folders shares the row this sets.
            sql::execute(
                conn,
                "UPDATE message SET preview = ?3
                  WHERE account = ?1 AND provider_key = ?2 AND preview IS NULL",
                (account.as_str(), key.as_str(), preview.as_str()),
            )?;
            Ok(())
        })
        .await
    }

    async fn get_message_body(
        &self,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<Option<MessageBody>> {
        let account = account.as_str().to_owned();
        let key = key.as_str().to_owned();
        self.read(move |conn| select_body(conn, &account, &key))
            .await
    }

    async fn mail_missing_body(
        &self,
        accounts: &[AccountId],
        limit: usize,
    ) -> Result<Vec<MailListRow>> {
        let accounts = accounts.to_vec();
        self.read(move |conn| mail_ops::mail_missing_body(conn, &accounts, limit))
            .await
    }
}

/// Upserts the metadata row mapping `(account, key)` to its blob's content hash.
fn upsert_source(
    conn: &Connection,
    account: &str,
    key: &str,
    hash: &str,
    fetched_at: &str,
) -> Result<()> {
    sql::execute(
        conn,
        "INSERT INTO message_source (account, provider_key, content_hash, fetched_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account, provider_key) DO UPDATE SET
             content_hash = excluded.content_hash,
             fetched_at   = excluded.fetched_at",
        (account, key, hash, fetched_at),
    )?;
    Ok(())
}

/// Reads the blob content hash recorded for `(account, key)`, if any.
fn select_hash(conn: &Connection, account: &str, key: &str) -> Result<Option<String>> {
    sql::query_opt(
        conn,
        "SELECT content_hash FROM message_source WHERE account = ?1 AND provider_key = ?2",
        (account, key),
        |row| row.get(0),
    )
}

/// Upserts the extracted body text for `(account, key)`; the `message_body_au`
/// trigger keeps `message_body_fts` in sync.
fn upsert_body(
    conn: &Connection,
    account: &str,
    key: &str,
    plain: &str,
    html: Option<&str>,
    fetched_at: &str,
) -> Result<()> {
    sql::execute(
        conn,
        "INSERT INTO message_body (account, provider_key, plain, html, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account, provider_key) DO UPDATE SET
             plain = excluded.plain, html = excluded.html, fetched_at = excluded.fetched_at",
        (account, key, plain, html, fetched_at),
    )?;
    Ok(())
}

/// Reads the cached body text for `(account, key)`, if any. An empty stored `plain`
/// maps back to "no plain part".
fn select_body(conn: &Connection, account: &str, key: &str) -> Result<Option<MessageBody>> {
    let row: Option<(String, Option<String>)> = sql::query_opt(
        conn,
        "SELECT plain, html FROM message_body WHERE account = ?1 AND provider_key = ?2",
        (account, key),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(row.map(|(plain, html)| {
        let plain = (!plain.is_empty()).then_some(plain);
        MessageBody::new(plain, html)
    }))
}
