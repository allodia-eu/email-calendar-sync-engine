//! Fetching a message's raw RFC 5322 source over an IMAP connection.
//!
//! The free function [`fetch_message_source`] drives a `&mut Connection<S>` so it is
//! generic over the stream (offline tests replay it over a mock), and
//! [`fetch_from_pool`] decides which of the account's connections it runs on. The
//! key→mailbox+UID resolution and the `UIDVALIDITY` guard are shared with the mutate path
//! via [`crate::target::examine_target`]; a body read opens the mailbox read-only
//! (`EXAMINE`), since peeking a body must not take a write-intent open or disturb
//! `\Recent`, and skips even that when the session already has the mailbox open.

use std::sync::Arc;

use engine_core::{error::FailureClass, ids::ProviderKey, raw::RawMime};
use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    mail::parse_message_key, pool::ImapPool, target::examine_target, transport::Connection,
};

/// Fetches the raw source of the message named by `key` from the account's `pool`.
///
/// It asks for a connection that already has the message's mailbox open, so a run of reads
/// in one folder pays one round trip each rather than two. Concurrent calls run on separate
/// connections up to the pool's worker capacity, which is what a host overlapping its fetches
/// is counting on.
///
/// A read that fails on a lost connection (a dropped or silent socket, a `BYE`) is tried once
/// more on a connection proved alive: a pooled connection can die between two uses, and a read
/// changes nothing on the server, so repeating it is safe. Anything else, a stale key included,
/// is the caller's to handle.
///
/// # Errors
///
/// As [`fetch_message_source`], from the second attempt when there was one; or the
/// classified dial failure when no connection could be had.
pub(crate) async fn fetch_from_pool<S>(
    pool: &Arc<ImapPool<S>>,
    key: &ProviderKey,
) -> ProviderResult<RawMime>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    let mailbox = parse_message_key(key.as_str()).map(|(mailbox, ..)| mailbox);
    let first = async {
        let mut connection = match mailbox {
            Some(mailbox) => pool.acquire_for(mailbox).await?,
            None => pool.acquire().await?,
        };
        let result = fetch_message_source(&mut connection, key).await;
        connection.settle(result)
    }
    .await;
    match first {
        Err(err) if err.class() == FailureClass::Retryable => {
            let mut connection = pool.acquire_checked().await?;
            let result = fetch_message_source(&mut connection, key).await;
            connection.settle(result)
        }
        other => other,
    }
}

/// Fetches the raw source of the message named by `key` over `connection`.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if `key` is not a parseable IMAP key, or its mailbox name
///   carries a control character.
/// - [`ProviderError::conflict`] if the target mailbox's `UIDVALIDITY` has changed since the key
///   was synthesized, **or** the UID no longer exists (expunged since the last sync) — either way
///   the caller re-syncs before retrying.
/// - A classified [`ProviderError`] from the underlying IMAP command on failure, a
///   [`Retryable`](FailureClass::Retryable) one when the server went silent mid-body for
///   [`BODY_READ_STALL`](crate::transport_read::BODY_READ_STALL).
pub(crate) async fn fetch_message_source<S>(
    connection: &mut Connection<S>,
    key: &ProviderKey,
) -> ProviderResult<RawMime>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (_mailbox, uid) = examine_target(connection, key).await?;
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
