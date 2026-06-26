//! Fetching a message's raw RFC 5322 source over an open IMAP connection.
//!
//! The free function [`fetch_message_source`] drives a `&mut Connection<S>` so it is
//! generic over the stream (offline tests replay it over a mock) and the `Provider`
//! impl stays a thin lock-and-call, mirroring [`crate::mutate`]. The key→mailbox+UID
//! resolution and the `UIDVALIDITY` guard are shared with the mutate path via
//! [`crate::target::select_target`]; a body read opens the mailbox read-only
//! ([`Access::ReadOnly`] → `EXAMINE`), since peeking a body must not take a
//! write-intent open or disturb `\Recent`.

use engine_core::ids::ProviderKey;
use engine_core::raw::RawMime;
use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::target::{Access, select_target};
use crate::transport::Connection;

/// Fetches the raw source of the message named by `key` over `connection`.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if `key` is not a parseable IMAP key, or its
///   mailbox name carries a control character.
/// - [`ProviderError::conflict`] if the target mailbox's `UIDVALIDITY` has changed
///   since the key was synthesized, **or** the UID no longer exists (expunged since
///   the last sync) — either way the caller re-syncs before retrying.
/// - A classified [`ProviderError`] from the underlying IMAP command on failure.
pub(crate) async fn fetch_message_source<S>(
    connection: &mut Connection<S>,
    key: &ProviderKey,
) -> ProviderResult<RawMime>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (_mailbox, uid) = select_target(connection, key, Access::ReadOnly).await?;
    let bytes = connection.uid_fetch_body(uid).await?.ok_or_else(|| {
        ProviderError::conflict(format!(
            "message UID {uid} no longer exists (expunged): re-sync before fetching"
        ))
    })?;
    Ok(RawMime::new(bytes))
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
