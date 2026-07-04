//! On-demand message attachment reads over the cached raw MIME source.

use engine_core::ids::AccountId;
use engine_core::mail::{AttachmentPartId, Message, MessageAttachment, MessageAttachmentContent};
use engine_core::raw::RawMime;
use engine_provider::Provider;
use engine_store::MessageSourceCache;

use crate::SyncError;

/// Returns downloadable attachment metadata for `message`.
///
/// Cache-first on the raw RFC 5322 source: the store's content-addressed blob if present,
/// otherwise one provider fetch. The raw source is cached best-effort after a provider fetch,
/// so opening the body and listing attachments share one download.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the source fetch fails or [`SyncError::Store`] if a
/// cache read fails.
pub async fn fetch_message_attachments<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
) -> Result<Vec<MessageAttachment>, SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    with_raw_source(
        provider,
        store,
        account,
        message,
        engine_mime::extract_attachments,
    )
    .await
}

/// Returns one decoded attachment selected by `id`.
///
/// This uses the same raw-source read-through cache as [`fetch_message_attachments`]. A
/// missing id returns `Ok(None)` because the source was readable but no downloadable part
/// matched that message-scoped id.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the source fetch fails or [`SyncError::Store`] if a
/// cache read fails.
pub async fn fetch_message_attachment<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
    id: AttachmentPartId,
) -> Result<Option<MessageAttachmentContent>, SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    with_raw_source(provider, store, account, message, |raw| {
        engine_mime::extract_attachment(raw, id)
    })
    .await
}

/// Reads the raw RFC 5322 source (cache-first: the content-addressed blob, else one provider
/// fetch) and hands it to `extract`.
///
/// On a provider fetch the raw is cached best-effort by **move** after `extract` has run — the
/// read already succeeded, so a cache-write failure never denies it, and no copy of a
/// potentially large source (the attachment-bearing messages are exactly the big ones) is made.
async fn with_raw_source<P, S, T>(
    provider: &P,
    store: &S,
    account: &AccountId,
    message: &Message,
    extract: impl FnOnce(&RawMime) -> T,
) -> Result<T, SyncError>
where
    P: Provider,
    S: MessageSourceCache,
{
    let key = message.id.key();
    if let Some(cached) = store.get_message_source(account, key).await? {
        return Ok(extract(&cached));
    }
    let raw = provider.fetch_message_source(account, message).await?;
    let out = extract(&raw);
    let _ = store.put_message_source(account, key, raw).await;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use engine_core::ids::{AccountId, MailboxId, MessageId};
    use engine_core::membership::Memberships;
    use engine_core::raw::RawMime;
    use engine_provider::{Capabilities, Provider, ProviderResult};
    use engine_store::{ManualClock, MessageSourceCache};
    use store_sqlite::SqliteStore;

    use super::*;

    struct AttachmentProvider {
        caps: Capabilities,
        raw: Vec<u8>,
        hits: AtomicUsize,
    }

    #[async_trait]
    impl Provider for AttachmentProvider {
        fn capabilities(&self) -> &Capabilities {
            &self.caps
        }

        async fn fetch_message_source(
            &self,
            _account: &AccountId,
            _message: &Message,
        ) -> ProviderResult<RawMime> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(RawMime::new(self.raw.clone()))
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

    const RAW: &[u8] = b"Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
        --m\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
        Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
        Content-Transfer-Encoding: base64\r\n\r\nUERG\r\n--m--\r\n";

    #[tokio::test]
    async fn attachments_fetch_cache_and_extract_metadata() {
        let provider = AttachmentProvider {
            caps: Capabilities::none().with_mail().with_message_source(),
            raw: RAW.to_vec(),
            hits: AtomicUsize::new(0),
        };
        let store = store();
        assert!(provider.capabilities().message_source());

        let attachments = fetch_message_attachments(&provider, &store, &account(), &message())
            .await
            .expect("attachments");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].file_name(), "report.pdf");
        assert_eq!(provider.hits.load(Ordering::SeqCst), 1);
        assert!(
            store
                .get_message_source(&account(), message().id.key())
                .await
                .expect("get cached raw")
                .is_some()
        );
    }

    #[tokio::test]
    async fn attachment_content_reuses_cached_raw() {
        let store = store();
        store
            .put_message_source(&account(), message().id.key(), RawMime::new(RAW.to_vec()))
            .await
            .expect("seed source");
        let provider = AttachmentProvider {
            caps: Capabilities::none().with_mail().with_message_source(),
            raw: b"unused".to_vec(),
            hits: AtomicUsize::new(0),
        };

        let content = fetch_message_attachment(
            &provider,
            &store,
            &account(),
            &message(),
            AttachmentPartId::new(0),
        )
        .await
        .expect("attachment")
        .expect("present");
        assert_eq!(content.bytes(), b"PDF");
        assert_eq!(provider.hits.load(Ordering::SeqCst), 0);
    }
}
