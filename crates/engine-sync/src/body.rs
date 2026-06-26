//! On-demand fetch of a message's body — a read-through cache, no lease.
//!
//! Unlike sync and the outbox, reading a body takes **no** scope or op lease: the
//! raw bytes are immutable Tier-3 content and the caches are idempotent
//! (`store-and-sync.md`), so a host can open a message while a sync of its scope is
//! in flight. The flow is cache-first in three tiers — the extracted text in SQLite,
//! else the cached raw bytes on disk, else one provider fetch — extracting the
//! displayable text with `engine-mime` and caching both halves best-effort.

use engine_core::ids::AccountId;
use engine_core::mail::{Message, MessageBody};
use engine_provider::Provider;
use engine_store::{MessageBodyStore, MessageSourceCache};

use crate::SyncError;

/// Returns the displayable [`MessageBody`] of `message`.
///
/// Cache-first, in three tiers: the extracted body **text** in SQLite (the fast
/// reading-view path — no disk read, no re-parse); else the cached raw **bytes** on
/// disk; else a one-time provider fetch of the whole raw message (which also serves
/// the later HTML/attachment slices without re-fetching). The newly-fetched bytes and
/// extracted text are cached **best-effort** — a cache-write failure never denies a
/// read of content already in hand.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the body fetch fails (a stale or expunged IMAP
/// target is a `Conflict` — re-sync, then retry), or [`SyncError::Store`] if a cache
/// **read** fails.
pub async fn fetch_message_body<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
) -> Result<MessageBody, SyncError>
where
    P: Provider,
    S: MessageSourceCache + MessageBodyStore,
{
    let key = message.id.key();
    // Fast path: the extracted text is already in SQLite.
    if let Some(body) = store.get_message_body(account, key).await? {
        return Ok(body);
    }

    // Otherwise we need the raw bytes — from the on-disk blob, or one provider fetch.
    let (from_provider, raw) = match store.get_message_source(account, key).await? {
        Some(cached) => (false, cached),
        None => (true, provider.fetch_message_source(account, message).await?),
    };
    let body = engine_mime::extract_body(&raw);

    // Best-effort caching; the read already succeeded.
    if from_provider {
        let _ = store.put_message_source(account, key, raw).await;
    }
    let _ = store.put_message_body(account, key, &body).await;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use engine_core::ids::{AccountId, MailboxId, MessageId};
    use engine_core::mail::{Message, MessageBody};
    use engine_core::membership::Memberships;
    use engine_core::raw::RawMime;
    use engine_provider::{Capabilities, Provider, ProviderResult};
    use engine_store::{ManualClock, MessageBodyStore, MessageSourceCache};
    use store_sqlite::SqliteStore;

    use super::fetch_message_body;

    /// A provider whose only ability is body fetch; it counts how often it is hit,
    /// so the cache-hit test can prove the second read never reaches the network.
    struct CountingProvider {
        caps: Capabilities,
        body: Vec<u8>,
        hits: AtomicUsize,
    }

    impl CountingProvider {
        fn new(body: &[u8]) -> Self {
            Self {
                caps: Capabilities::none().with_mail().with_message_source(),
                body: body.to_vec(),
                hits: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Provider for CountingProvider {
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }

        async fn fetch_message_source(
            &self,
            _account: &AccountId,
            _message: &Message,
        ) -> ProviderResult<RawMime> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(RawMime::new(self.body.clone()))
        }
    }

    fn account() -> AccountId {
        AccountId::try_from("acct").expect("account")
    }

    fn message() -> Message {
        Message::new(
            MessageId::try_from("imap:v1:u1@INBOX").expect("id"),
            Memberships::of_one(MailboxId::try_from("INBOX").expect("mailbox")),
        )
    }

    fn store() -> SqliteStore<ManualClock> {
        SqliteStore::open_in_memory(ManualClock::new(
            "2026-06-26T00:00:00Z".parse().expect("instant"),
        ))
        .expect("store")
    }

    const RAW: &[u8] = b"Content-Type: text/plain\r\n\r\nthe decoded body";

    #[tokio::test]
    async fn cache_miss_fetches_caches_and_extracts() {
        let provider = CountingProvider::new(RAW);
        let store = store();

        assert!(provider.capabilities().message_source());
        let body = fetch_message_body(&provider, &store, &account(), &message())
            .await
            .expect("fetch body");
        assert!(body.plain().unwrap().contains("the decoded body"));
        assert_eq!(provider.hits.load(Ordering::SeqCst), 1, "fetched once");

        // Both the raw bytes and the extracted text are now cached.
        assert!(
            store
                .get_message_source(&account(), message().id.key())
                .await
                .expect("get source")
                .is_some()
        );
        assert!(
            store
                .get_message_body(&account(), message().id.key())
                .await
                .expect("get body")
                .is_some()
        );
    }

    #[tokio::test]
    async fn raw_cached_extracts_without_a_provider_fetch() {
        let store = store();
        // Raw bytes cached but text not yet extracted: the read uses the on-disk
        // blob, so the counting provider is never consulted.
        store
            .put_message_source(&account(), message().id.key(), RawMime::new(RAW.to_vec()))
            .await
            .expect("seed source");

        let provider = CountingProvider::new(b"unused - should not be fetched");
        let body = fetch_message_body(&provider, &store, &account(), &message())
            .await
            .expect("fetch body from blob");
        assert!(body.plain().unwrap().contains("the decoded body"));
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            0,
            "served from disk blob"
        );
    }

    #[tokio::test]
    async fn body_text_cached_skips_blob_and_provider() {
        let store = store();
        // The extracted text is cached: the fast path returns it directly — no blob
        // read, no provider fetch.
        let seeded = MessageBody::new(Some("the fast-path body".to_owned()), None);
        store
            .put_message_body(&account(), message().id.key(), &seeded)
            .await
            .expect("seed body");

        let provider = CountingProvider::new(b"unused - should not be fetched");
        let body = fetch_message_body(&provider, &store, &account(), &message())
            .await
            .expect("fetch body from sqlite");
        assert_eq!(body.plain(), Some("the fast-path body"));
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            0,
            "served from sqlite"
        );
    }

    #[tokio::test]
    async fn provider_error_propagates() {
        // A provider with no body-fetch capability rejects; the error surfaces as a
        // provider sync error rather than a panic or a silent empty body.
        struct Unsupported {
            caps: Capabilities,
        }
        #[async_trait]
        impl Provider for Unsupported {
            fn capabilities(&self) -> &Capabilities {
                &self.caps
            }
        }
        let provider = Unsupported {
            caps: Capabilities::none().with_mail(),
        };
        assert!(!provider.capabilities().message_source());
        let err = fetch_message_body(&provider, &store(), &account(), &message())
            .await
            .unwrap_err();
        assert!(matches!(err, crate::SyncError::Provider(_)));
    }
}
