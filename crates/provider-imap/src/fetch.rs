//! Fetching a message's raw RFC 5322 source over an open IMAP connection.
//!
//! The free function [`fetch_message_source`] drives a `&mut Connection<S>` so it is
//! generic over the stream (offline tests replay it over a mock) and the `Provider`
//! impl stays a thin lock-and-call, mirroring [`crate::mutate`]. It addresses the
//! message by its key, not the provider's bound mailbox, so one connected provider
//! can read a body from any of the account's folders.
//!
//! IMAP identity is `(mailbox, UIDVALIDITY, UID)`, so the key's mailbox is
//! `SELECT`ed and its returned `UIDVALIDITY` is checked against the key's: a mismatch
//! means the UID space was renumbered and every prior key is stale, so the fetch is a
//! [`ProviderError::conflict`] (the caller re-syncs, then retries) rather than a read
//! of the wrong message.

use engine_core::ids::ProviderKey;
use engine_core::raw::RawMime;
use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::mail::parse_message_key;
use crate::mutate::reject_control_chars;
use crate::transport::Connection;

/// Fetches the raw source of the message named by `key` over `connection`.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if `key` is not a parseable IMAP key, or its
///   mailbox name carries a control character (`CR`/`LF`/`NUL`) that could inject a
///   second command line.
/// - [`ProviderError::conflict`] if the target mailbox's `UIDVALIDITY` has changed
///   since the key was synthesized (the key is stale; re-sync before fetching).
/// - A classified [`ProviderError`] from the underlying IMAP command on failure.
pub(crate) async fn fetch_message_source<S>(
    connection: &mut Connection<S>,
    key: &ProviderKey,
) -> ProviderResult<RawMime>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mailbox, key_validity, uid) = parse_message_key(key.as_str()).ok_or_else(|| {
        ProviderError::invalid_state(format!("unparseable IMAP message key: {}", key.as_str()))
    })?;
    reject_control_chars(mailbox)?;

    // SELECT the key's own mailbox and guard on UIDVALIDITY: a renumbered UID space
    // invalidates the key, so a fetch would read the wrong message — surface a
    // Conflict so the caller re-syncs first.
    let selected = connection.select(mailbox).await?;
    if selected.uid_validity != key_validity {
        return Err(ProviderError::conflict(format!(
            "UIDVALIDITY changed for {mailbox}: re-sync before fetching"
        )));
    }

    let bytes = connection.uid_fetch_body(&uid.to_string()).await?;
    Ok(RawMime::new(bytes))
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
