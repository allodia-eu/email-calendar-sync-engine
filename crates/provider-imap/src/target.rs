//! Resolving an IMAP message key to an open mailbox + UID, shared by the read
//! ([`crate::fetch`]) and write ([`crate::mutate`]) paths.
//!
//! IMAP identity is `(mailbox, UIDVALIDITY, UID)`. Both paths must parse the key,
//! reject a control-char mailbox name (a `CR`/`LF`/`NUL` would inject a second
//! command line), open the key's mailbox, and guard its `UIDVALIDITY` against the
//! key's — a mismatch means the UID space was renumbered and every prior key is
//! stale, so the operation is a [`ProviderError::conflict`] (re-sync, then retry)
//! rather than touching the wrong message. Keeping this in one place means a future
//! hardening of the guard cannot drift between the two paths.

use engine_core::ids::ProviderKey;
use engine_provider::{ProviderError, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{mail::parse_message_key, parse::SelectData, transport::Connection};

/// Parses `key`, rejects a control-char mailbox, `SELECT`s that mailbox, guards its
/// `UIDVALIDITY`, and returns the borrowed mailbox name, the UID and what the `SELECT` read.
/// Always a fresh `SELECT`, whatever the session has open: a write needs write access, and a
/// caller may read the counts the response carries.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if `key` is not a parseable IMAP key or its mailbox name
///   carries a control character.
/// - [`ProviderError::conflict`] if the mailbox's `UIDVALIDITY` no longer matches the key's (stale
///   key; re-sync first).
/// - A classified [`ProviderError`] from the underlying `SELECT`.
pub(crate) async fn select_target<'k, S>(
    connection: &mut Connection<S>,
    key: &'k ProviderKey,
) -> ProviderResult<(&'k str, u32, SelectData)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mailbox, key_validity, uid) = parse_target(key)?;
    let selected = connection.select(mailbox).await?;
    guard_validity(mailbox, selected.uid_validity, key_validity)?;
    Ok((mailbox, uid, selected))
}

/// [`select_target`] for a read: the mailbox is opened read-only (`EXAMINE`), so the peek takes
/// no write-intent open, leaves `\Recent` untouched and works on a read-only folder, and it is
/// not opened at all when the session already has it open (`crate::transport_select`). Returns
/// the mailbox name and the UID.
///
/// # Errors
///
/// As [`select_target`], from `EXAMINE`.
pub(crate) async fn examine_target<'k, S>(
    connection: &mut Connection<S>,
    key: &'k ProviderKey,
) -> ProviderResult<(&'k str, u32)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mailbox, key_validity, uid) = parse_target(key)?;
    let validity = match connection.open_validity(mailbox) {
        Some(validity) => validity,
        None => connection.examine(mailbox).await?.uid_validity,
    };
    guard_validity(mailbox, validity, key_validity)?;
    Ok((mailbox, uid))
}

/// The mailbox, `UIDVALIDITY` and UID a key names, refused when it is unparseable or its
/// mailbox name could inject a command.
fn parse_target(key: &ProviderKey) -> ProviderResult<(&str, u32, u32)> {
    let (mailbox, key_validity, uid) = parse_message_key(key.as_str()).ok_or_else(|| {
        ProviderError::invalid_state(format!("unparseable IMAP message key: {}", key.as_str()))
    })?;
    reject_control_chars(mailbox)?;
    Ok((mailbox, key_validity, uid))
}

/// A mismatch means the UID space was renumbered and every prior key is stale.
fn guard_validity(mailbox: &str, open: u32, key_validity: u32) -> ProviderResult<()> {
    if open != key_validity {
        return Err(ProviderError::conflict(format!(
            "UIDVALIDITY changed for {mailbox}: re-sync before retrying"
        )));
    }
    Ok(())
}

/// Rejects a mailbox name carrying `CR`/`LF`/`NUL` before it reaches a quoted IMAP
/// command argument: those bytes cannot appear in a valid mailbox name, and admitting
/// them would let a crafted name inject a second command line (the transport's `quote`
/// escapes only `"`/`\`). Shared with the move-destination check in [`crate::mutate`].
pub(crate) fn reject_control_chars(name: &str) -> ProviderResult<()> {
    if name.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(ProviderError::invalid_state(
            "mailbox name contains a control character",
        ));
    }
    Ok(())
}
