//! On-demand fetch of a message's body — a read-through cache, no lease.
//!
//! Unlike sync and the outbox, reading a body takes **no** scope or op lease: the
//! raw bytes are immutable Tier-3 content and the cache is idempotent
//! (`store-and-sync.md`), so a host can open a message while a sync of its scope is
//! in flight. The flow is cache-first — return the cached raw, else fetch it from
//! the provider once and cache it — then extract the displayable text with
//! `engine-mime`.

use engine_core::ids::AccountId;
use engine_core::mail::{Message, MessageBody};
use engine_provider::Provider;
use engine_store::MessageSourceCache;

use crate::SyncError;

/// Returns the displayable [`MessageBody`] of `message`, fetching and caching its
/// raw RFC 5322 source on the first call and serving it from the cache thereafter.
///
/// The cached raw is the whole message (headers + every part), so this one fetch
/// also serves the later HTML and attachment slices without re-fetching.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the body fetch fails (a stale IMAP target is a
/// `Conflict` — re-sync, then retry), or [`SyncError::Store`] if the cache read or
/// write fails.
pub async fn fetch_message_body<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
) -> Result<MessageBody, SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    let key = message.id.key();
    let raw = if let Some(cached) = store.get_message_source(account, key).await? {
        cached
    } else {
        let fetched = provider.fetch_message_source(account, message).await?;
        store.put_message_source(account, key, &fetched).await?;
        fetched
    };
    Ok(engine_mime::extract_body(&raw))
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use engine_core::ids::{AccountId, MailboxId, MessageId};
    use engine_core::mail::Message;
    use engine_core::membership::Memberships;
    use engine_core::raw::RawMime;
    use engine_provider::{Capabilities, Provider, ProviderResult};
    use engine_store::{ManualClock, MessageSourceCache};
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

        // The raw is now in the cache.
        assert!(
            store
                .get_message_source(&account(), message().id.key())
                .await
                .expect("get")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cache_hit_does_not_fetch() {
        let store = store();
        // Pre-seed the cache, then prove the read is served from it: the counting
        // provider records zero hits, so no network round trip happened.
        store
            .put_message_source(&account(), message().id.key(), &RawMime::new(RAW.to_vec()))
            .await
            .expect("seed");

        let provider = CountingProvider::new(b"unused - should not be fetched");
        let body = fetch_message_body(&provider, &store, &account(), &message())
            .await
            .expect("fetch body from cache");
        assert!(body.plain().unwrap().contains("the decoded body"));
        assert_eq!(provider.hits.load(Ordering::SeqCst), 0, "served from cache");
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
