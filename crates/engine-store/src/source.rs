//! On-demand message-content caches: raw bytes on the filesystem, body text in the
//! relational store (`store-and-sync.md`, the "text vs bytes" split).
//!
//! Both traits are **deliberately outside** the [`Store`](crate::Store)
//! scope-fencing/lease contract: a message's raw bytes (for a fixed
//! `(UIDVALIDITY, UID)` or JMAP blob) are immutable, and the extracted text is a pure
//! function of them, so the caches are idempotent and need no lease — a host can open
//! and search a message while a sync of the same scope is in flight.

use async_trait::async_trait;
use engine_core::ids::{AccountId, ProviderKey};
use engine_core::mail::MessageBody;
use engine_core::raw::RawMime;

use crate::error::Result;

/// A content cache for raw message sources — the Tier-3 *bytes* a host fetches on
/// demand (the whole RFC 5322 message, which carries its attachments).
///
/// Backends keep the (potentially multi-megabyte) bytes **out** of the relational
/// store — `store-sqlite` writes them to a content-addressed filesystem blob area and
/// keeps only metadata — so a large attachment never bloats the database.
#[async_trait]
pub trait MessageSourceCache {
    /// Caches `source` as the raw bytes of the message identified by
    /// `(account, key)`, replacing any prior entry. Takes ownership so a large
    /// message moves into the blob writer rather than being copied. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`](crate::StoreError) on a backend failure (database or
    /// blob-area I/O).
    async fn put_message_source(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        source: RawMime,
    ) -> Result<()>;

    /// Returns the cached raw source for `(account, key)`, or `None` if it has not
    /// been fetched (or its backing blob is missing or fails its content-hash check,
    /// so a caller re-fetches).
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

/// A cache for a message's extracted, displayable body *text* — the reading view and
/// the search source.
///
/// `store-sqlite` stores it in SQLite (small, searchable) and maintains a lease-free
/// FTS index over the plain text, so a search matches body content. Sync never
/// touches it, so an IMAP re-snapshot cannot wipe it.
#[async_trait]
pub trait MessageBodyStore {
    /// Caches the extracted `body` text for `(account, key)`, replacing any prior
    /// entry and refreshing its search index. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`](crate::StoreError) on a backend failure.
    async fn put_message_body(
        &self,
        account: &AccountId,
        key: &ProviderKey,
        body: &MessageBody,
    ) -> Result<()>;

    /// Returns the cached body text for `(account, key)`, or `None` if no body has
    /// been extracted yet.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`](crate::StoreError) on a backend failure.
    async fn get_message_body(
        &self,
        account: &AccountId,
        key: &ProviderKey,
    ) -> Result<Option<MessageBody>>;
}
