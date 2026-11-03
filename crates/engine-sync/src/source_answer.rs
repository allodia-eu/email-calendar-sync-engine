//! A message's paperclip, answered from its own source (`store-and-sync.md`, "The source answers
//! the row's paperclip").
//!
//! Every road that holds a message's source goes through here, so none of them can forget the
//! answer: a source a provider just returned is answered as it enters the cache
//! ([`cache_fetched_source`]), and one already cached answers once if it still owes it
//! ([`answer_if_owed`], [`answer_from_if_owed`]) — the case of a source cached before answers
//! were recorded. All of it is best-effort, like the caches it sits beside: a failed write
//! leaves the provider's guess on the row, never fails the read.

use engine_core::{
    ids::{AccountId, ProviderKey},
    raw::RawMime,
};
use engine_store::MessageSourceCache;

/// Caches `raw`, the source a provider just returned for `key`, answering its paperclip first.
pub(crate) async fn cache_fetched_source<S>(
    store: &S,
    account: &AccountId,
    key: &ProviderKey,
    raw: RawMime,
) where
    S: MessageSourceCache + ?Sized,
{
    answer_from(store, account, key, &raw).await;
    let _ = store.put_message_source(account, key, raw).await;
}

/// Answers `key` from `raw`, a source already cached, if no answer was recorded for it yet.
pub(crate) async fn answer_from_if_owed<S>(
    store: &S,
    account: &AccountId,
    key: &ProviderKey,
    raw: &RawMime,
) where
    S: MessageSourceCache + ?Sized,
{
    if owes_answer(store, account, key).await {
        answer_from(store, account, key, raw).await;
    }
}

/// Answers `key` from its cached source if it still owes an answer, for a read that never
/// touched the source (a body served from its cached text). Reads the blob at most once per
/// message, and never asks the provider.
pub(crate) async fn answer_if_owed<S>(store: &S, account: &AccountId, key: &ProviderKey)
where
    S: MessageSourceCache + ?Sized,
{
    if owes_answer(store, account, key).await
        && let Ok(Some(raw)) = store.get_message_source(account, key).await
    {
        answer_from(store, account, key, &raw).await;
    }
}

async fn owes_answer<S>(store: &S, account: &AccountId, key: &ProviderKey) -> bool
where
    S: MessageSourceCache + ?Sized,
{
    matches!(store.mail_has_attachment(account, key).await, Ok(None))
}

/// Records what `raw` says; bytes that are no message say nothing, so nothing is recorded.
async fn answer_from<S>(store: &S, account: &AccountId, key: &ProviderKey, raw: &RawMime)
where
    S: MessageSourceCache + ?Sized,
{
    if let Some(has_attachment) = engine_mime::has_downloadable_attachment(raw) {
        let _ = store
            .set_mail_has_attachment(account, key, has_attachment)
            .await;
    }
}

#[cfg(test)]
#[path = "source_answer_tests.rs"]
mod tests;
