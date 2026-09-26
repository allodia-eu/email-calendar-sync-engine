//! Warming many messages' caches in one provider request ([`warm_message_sources`]).
//!
//! [`fetch_message_body`](crate::fetch_message_body) is built for an open: one message,
//! cache-first. A warm is the other shape: thousands of messages the caller already knows are
//! not cached, where one provider round trip per message is the whole cost. This asks the
//! provider for a batch at once ([`Provider::fetch_message_sources`]) and caches each message
//! the moment its bytes arrive, exactly as an open would have.

use engine_core::{ids::AccountId, mail::Message};
use engine_provider::Provider;
use engine_store::{MessageBodyStore, MessageSourceCache};
use futures_util::{Stream, StreamExt};

use crate::SyncError;

/// Fetches the raw sources of `messages` in as few provider requests as the transport allows,
/// caching each one's bytes, extracted text and derived list snippet as it arrives, and yields
/// `(index into messages, outcome)` for every message, in the order they complete.
///
/// For messages the caller knows are not warm, such as a page of
/// [`MailStore::mail_missing_body`](engine_store::MailStore): nothing is looked up first, so a
/// message that was already cached is fetched again. The caches are written best-effort, like
/// every read-through cache here: a failed write leaves the message on the work list for the
/// next pass rather than failing a fetch that succeeded. Takes no lease.
///
/// A message's failure is its own: a stale or expunged IMAP target is a `Conflict` (re-sync,
/// then retry), and the other messages of the batch are unaffected.
pub fn warm_message_sources<'a, P, S>(
    provider: &'a P,
    store: &'a S,
    account: &'a AccountId,
    messages: &'a [Message],
) -> impl Stream<Item = (usize, Result<(), SyncError>)> + Send + 'a
where
    P: Provider + ?Sized,
    S: MessageSourceCache + MessageBodyStore + Sync,
{
    provider
        .fetch_message_sources(account, messages)
        .then(move |(index, fetched)| async move {
            let outcome = match fetched {
                Ok(raw) => {
                    let message = &messages[index];
                    let key = message.id.key();
                    let body = engine_mime::extract_body(&raw);
                    let _ = store.put_message_source(account, key, raw).await;
                    let _ = store.put_message_body(account, key, &body).await;
                    // The same gate an open applies: only a message with no snippet of its own
                    // gets one derived from its body.
                    if message.preview.is_none()
                        && let Some(preview) = engine_mime::preview_from_body(&body)
                    {
                        let _ = store.set_mail_preview(account, key, &preview).await;
                    }
                    Ok(())
                }
                Err(err) => Err(SyncError::from(err)),
            };
            (index, outcome)
        })
}

#[cfg(test)]
#[path = "warm_tests.rs"]
mod tests;
