//! Fetching several messages' raw sources in one call
//! ([`Provider::fetch_message_sources`]).
//!
//! A body warm reads thousands of messages. Asked for one at a time, each costs a round trip
//! of latency that nothing overlaps, and that latency, not the bytes, is what a first sync
//! spends its time on. A transport that can carry many sources in one request (an IMAP
//! `UID FETCH` over a UID set) streams them back as they arrive; one that cannot keeps the
//! default here and lets the caller overlap single fetches instead
//! ([`ConnectionInfo::concurrent_fetches`](crate::ConnectionInfo::concurrent_fetches)).

use engine_core::{ids::AccountId, mail::Message, raw::RawMime};
use futures_core::Stream;
use futures_util::StreamExt;

use crate::{Provider, ProviderResult};

/// The stream [`Provider::fetch_message_sources`] returns: each item is the index of a
/// message in the slice it was given, and that message's source or the reason there is none.
pub type SourceStream<'a> =
    core::pin::Pin<Box<dyn Stream<Item = (usize, ProviderResult<RawMime>)> + Send + 'a>>;

/// Fetches `messages` one after another with [`Provider::fetch_message_source`]: the default
/// for a transport with nothing better.
pub fn one_at_a_time<'a, P: Provider + ?Sized>(
    provider: &'a P,
    account: &'a AccountId,
    messages: &'a [Message],
) -> SourceStream<'a> {
    Box::pin(
        futures_util::stream::iter(messages.iter().enumerate()).then(
            move |(index, message)| async move {
                (index, provider.fetch_message_source(account, message).await)
            },
        ),
    )
}
