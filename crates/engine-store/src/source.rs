//! On-demand cache of a message's raw RFC 5322 source (Tier-3 bodies).

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::raw::RawMime;

use crate::error::Result;

/// A content cache for raw message sources — the Tier-3 blobs a host fetches on
/// demand to read a body and (later) attachments (`north-star.md`).
///
/// It is **deliberately outside** the [`Store`](crate::Store) scope-fencing/lease
/// contract: the raw bytes for a `(UIDVALIDITY, UID)` (or a JMAP blob) are
/// immutable, so two concurrent fetches write identical bytes — the cache is
/// idempotent and needs no lease, which lets a host open a message while a sync of
/// the same scope is in flight. Backends keep the (potentially multi-megabyte,
/// attachment-bearing) bytes out of the relational store — `store-sqlite` writes
/// them to a content-addressed filesystem blob area and keeps only metadata in
/// SQLite — so a large attachment never bloats the database.
#[async_trait]
pub trait MessageSourceCache {
    /// Caches `source` as the raw bytes of the message identified by
    /// `(account, key)`, replacing any prior entry. Idempotent: re-storing the same
    /// bytes is a no-op beyond refreshing the fetch timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`](crate::StoreError) on a backend failure (database or
    /// blob-area I/O).
    async fn put_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        source: &RawMime,
    ) -> Result<()>;

    /// Returns the cached raw source for `(account, key)`, or `None` if it has not
    /// been fetched (or its backing blob is missing, so a caller re-fetches).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`](crate::StoreError) on a backend failure.
    async fn get_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<Option<RawMime>>;
}
