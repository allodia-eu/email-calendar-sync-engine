//! On-demand fetch of a message's body — a read-through cache, no lease.
//!
//! Unlike sync and the outbox, reading a body takes **no** scope or op lease: the
//! raw bytes are immutable Tier-3 content and the caches are idempotent
//! (`store-and-sync.md`), so a host can open a message while a sync of its scope is
//! in flight. The flow is cache-first in three tiers — the extracted text in SQLite,
//! else the cached raw bytes on disk, else one provider fetch — extracting the
//! displayable text with `engine-mime` and caching both halves best-effort.

use engine_core::{
    ids::AccountId,
    mail::{InlinePart, Message, MessageBody},
};
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
    let cached = store.get_message_body(account, key).await?;
    let body = if let Some(body) = cached {
        body
    } else {
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
        body
    };

    // **The one place a derived list snippet is recorded**, and only for a message that has no
    // snippet of its own. JMAP, Graph and Gmail all send one, so for them this is a field test
    // and nothing more — no derivation, no store round trip. IMAP sends none, so the snippet has
    // to come from the body, and this is the only function that holds one.
    //
    // It is here rather than in the sync so that *every* road reaches it: a backfill's rows, a
    // delta's new arrival, and a message opened on demand all end up fetching their body. The
    // alternative — reading bodies during the sync — is an extra fetch per message on exactly
    // the pass that must not do one.
    //
    // The gate is the message's own preview rather than the stored row's, which costs no read
    // and is right for every message that has never had one. It does mean re-deriving on a
    // *repeat* open of an IMAP message, because the payload keeps the `None` the provider sent
    // while the row now has a snippet; the store's `preview IS NULL` catches that write. Bounded
    // by how often someone reopens a message, against a store read on every body fetched.
    if message.preview.is_none()
        && let Some(preview) = engine_mime::preview_from_body(&body)
    {
        let _ = store.set_mail_preview(account, key, &preview).await;
    }
    Ok(body)
}

/// Returns the inline (`cid:`-referenced) parts of `message` — the decoded bytes a host
/// inlines for `<img src="cid:…">` references in the rendered HTML body.
///
/// Cache-first on the **raw** bytes: the on-disk blob (cached by an earlier
/// [`fetch_message_body`] or a prior call), else one provider fetch — then decodes the
/// inline parts with [`engine_mime::extract_inline_parts`]. Unlike [`fetch_message_body`]
/// it does **not** read or write the SQLite body-text cache: inline attachment bytes are
/// kept out of the relational store ([`MessageSourceCache`] doc), so they are re-derived
/// from the immutable raw on demand (cheap). Lease-free, for the same reason as
/// [`fetch_message_body`] — the raw bytes and their decoding are immutable.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the source fetch fails (a stale or expunged IMAP
/// target is a `Conflict` — re-sync, then retry), or [`SyncError::Store`] if a cache
/// **read** fails.
pub async fn fetch_inline_parts<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
) -> Result<Vec<InlinePart>, SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    let key = message.id.key();
    let (from_provider, raw) = match store.get_message_source(account, key).await? {
        Some(cached) => (false, cached),
        None => (true, provider.fetch_message_source(account, message).await?),
    };
    let parts = engine_mime::extract_inline_parts(&raw);

    // Best-effort: re-cache the raw so a later body/inline read hits the blob. The read
    // already succeeded, so a cache-write failure never denies it.
    if from_provider {
        let _ = store.put_message_source(account, key, raw).await;
    }
    Ok(parts)
}

/// Ensures the raw source for `message` is cached, fetching it once if it is not — the half of
/// a warm that [`fetch_message_body`] cannot do on its own.
///
/// A body fetch is **text-first**: once the extracted text is cached it returns without reading
/// or fetching the bytes, which is right for an open and wrong for a warm. A message can hold
/// one without the other — that is exactly what dropping cached sources over a size cap leaves
/// behind — and such a message would otherwise sit on the work list for ever, looked at by every
/// pass and fixed by none.
///
/// Cheap where there is nothing to do: one indexed metadata read, no blob read, no decode.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the source fetch fails (a stale or expunged IMAP target is
/// a `Conflict` — re-sync, then retry), or [`SyncError::Store`] if the cache **read** fails.
pub async fn ensure_message_source<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
) -> Result<(), SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    let key = message.id.key();
    if store.get_message_source(account, key).await?.is_some() {
        return Ok(());
    }
    let raw = provider.fetch_message_source(account, message).await?;
    // Best-effort, like every other cache write here: the caller asked for the bytes to be
    // kept, not for a guarantee, and a failed write leaves the message on the work list.
    let _ = store.put_message_source(account, key, raw).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use engine_core::{
        ids::{AccountId, MailboxId, MessageId},
        mail::{Message, MessageBody},
        membership::Memberships,
        raw::RawMime,
    };
    use engine_provider::{Capabilities, ConnectionInfo, Provider, ProviderResult};
    use engine_store::{ManualClock, MessageBodyStore, MessageSourceCache};
    use store_sqlite::SqliteStore;

    use super::{ensure_message_source, fetch_inline_parts, fetch_message_body};

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
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(self.caps)
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

        assert!(provider.connection_info().capabilities.message_source());
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
            fn connection_info(&self) -> ConnectionInfo {
                ConnectionInfo::new(self.caps)
            }
        }
        let provider = Unsupported {
            caps: Capabilities::none().with_mail(),
        };
        assert!(!provider.connection_info().capabilities.message_source());
        let err = fetch_message_body(&provider, &store(), &account(), &message())
            .await
            .unwrap_err();
        assert!(matches!(err, crate::SyncError::Provider(_)));
    }

    // A `multipart/related` whose HTML references an inline image by `cid:`; `aGVsbG8=` is
    // base64 for `hello`, so the decoded inline bytes are easy to assert.
    const RAW_RELATED: &[u8] = b"Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
        --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@x\">\r\n\
        --b\r\nContent-Type: image/png\r\nContent-ID: <logo@x>\r\n\
        Content-Transfer-Encoding: base64\r\nContent-Disposition: inline\r\n\r\naGVsbG8=\r\n\
        --b--\r\n";

    #[tokio::test]
    async fn inline_parts_cache_miss_fetches_and_decodes() {
        let provider = CountingProvider::new(RAW_RELATED);
        let store = store();

        let parts = fetch_inline_parts(&provider, &store, &account(), &message())
            .await
            .expect("fetch inline parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].content_id(), "logo@x");
        assert_eq!(parts[0].media_type(), "image/png");
        assert_eq!(parts[0].bytes(), b"hello");
        assert_eq!(provider.hits.load(Ordering::SeqCst), 1, "fetched once");

        // The raw bytes are now cached for a later body/inline read.
        assert!(
            store
                .get_message_source(&account(), message().id.key())
                .await
                .expect("get source")
                .is_some()
        );
    }

    #[tokio::test]
    async fn inline_parts_served_from_cached_raw_without_provider_fetch() {
        let store = store();
        // Raw bytes already on disk (e.g. cached by a prior body read): inline-part
        // extraction reuses the blob and never reaches the provider.
        store
            .put_message_source(
                &account(),
                message().id.key(),
                RawMime::new(RAW_RELATED.to_vec()),
            )
            .await
            .expect("seed source");

        let provider = CountingProvider::new(b"unused - should not be fetched");
        let parts = fetch_inline_parts(&provider, &store, &account(), &message())
            .await
            .expect("fetch inline parts from blob");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].bytes(), b"hello");
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            0,
            "served from disk blob"
        );
    }

    #[tokio::test]
    async fn ensure_source_fetches_only_when_the_blob_is_missing() {
        let store = store();
        let provider = CountingProvider::new(RAW);

        ensure_message_source(&provider, &store, &account(), &message())
            .await
            .expect("first ensure");
        assert_eq!(provider.hits.load(Ordering::SeqCst), 1, "fetched once");
        assert!(
            store
                .get_message_source(&account(), message().id.key())
                .await
                .expect("read back")
                .is_some(),
            "and cached what it fetched",
        );

        ensure_message_source(&provider, &store, &account(), &message())
            .await
            .expect("second ensure");
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            1,
            "a cached source costs no provider call",
        );
    }

    #[tokio::test]
    async fn ensure_source_restores_a_body_whose_text_outlived_its_bytes() {
        // What a lowered size cap leaves: the extracted text, no blob. A body fetch is
        // text-first, so it would return happily and fetch nothing — this is the call that
        // puts the bytes back.
        let store = store();
        let seed = CountingProvider::new(RAW);
        fetch_message_body(&seed, &store, &account(), &message())
            .await
            .expect("warm");
        store
            .drop_message_sources_over(&account(), 0)
            .await
            .expect("drop the source, keep the text");

        let provider = CountingProvider::new(RAW);
        fetch_message_body(&provider, &store, &account(), &message())
            .await
            .expect("body still reads");
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            0,
            "text-first: the body read cannot notice the missing bytes",
        );

        ensure_message_source(&provider, &store, &account(), &message())
            .await
            .expect("ensure");
        assert_eq!(
            provider.hits.load(Ordering::SeqCst),
            1,
            "the bytes are back"
        );
    }

    #[tokio::test]
    async fn inline_parts_provider_error_propagates() {
        struct Unsupported {
            caps: Capabilities,
        }
        #[async_trait]
        impl Provider for Unsupported {
            fn connection_info(&self) -> ConnectionInfo {
                ConnectionInfo::new(self.caps)
            }
        }
        let provider = Unsupported {
            caps: Capabilities::none().with_mail(),
        };
        let err = fetch_inline_parts(&provider, &store(), &account(), &message())
            .await
            .unwrap_err();
        assert!(matches!(err, crate::SyncError::Provider(_)));
    }
}
