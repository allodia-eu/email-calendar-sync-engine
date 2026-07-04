//! Applying a [`MailEdit`] to its target message over an open IMAP connection.
//!
//! The free function [`edit_mail`] drives a `&mut Connection<S>` so it is generic
//! over the stream (the offline tests replay it over a mock) and the `Provider` impl
//! stays a thin lock-and-call. It maps the three provider-neutral edits onto IMAP:
//! `SetKeywords` → `UID STORE ±FLAGS.SILENT`, `MoveTo` → `UID MOVE` (RFC 6851),
//! `Delete` → `UID STORE +FLAGS (\Deleted)` then `UID EXPUNGE` (UIDPLUS, RFC 4315).
//! The target mailbox comes from the message key, not the provider's bound mailbox,
//! so one connected provider can edit a message in any of the account's folders.
//!
//! IMAP identity is `(mailbox, UIDVALIDITY, UID)`, so before any mutation the target
//! key's mailbox is `SELECT`ed and its returned `UIDVALIDITY` is checked against the
//! key's: a mismatch means the UID space was renumbered and every prior key is
//! stale, so the edit is a [`ProviderError::conflict`] (the caller re-syncs, then
//! retries) rather than a blind write against the wrong message.

use engine_provider::{MailEdit, MailEditReceipt, ProviderResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::mail::keyword_to_flag;
use crate::target::{Access, reject_control_chars, select_target};
use crate::transport::Connection;

/// Applies `edit` to its target message over `connection`, returning a receipt
/// carrying the edited message's key.
///
/// # Errors
///
/// - [`ProviderError::invalid_state`] if the target key is not a parseable IMAP key,
///   or a mailbox name (the key's source folder, or a move destination) contains a
///   control character — IMAP mailbox names cannot, and admitting `CR`/`LF`/`NUL`
///   would let it inject a second command into the protocol stream.
/// - [`ProviderError::conflict`] if the target mailbox's `UIDVALIDITY` has changed
///   since the key was synthesized (the key is stale; re-sync before editing).
/// - A classified [`ProviderError`] from the underlying IMAP command on failure.
pub(crate) async fn edit_mail<S>(
    connection: &mut Connection<S>,
    edit: &MailEdit,
) -> ProviderResult<MailEditReceipt>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let key = edit.target();
    // Resolve + SELECT the key's own mailbox (a move's source, a delete's home,
    // read-write since this mutates) and guard `UIDVALIDITY` — shared with the read
    // path so the stale-key and CR/LF-injection guards cannot drift apart.
    let (_mailbox, uid) = select_target(connection, key, Access::ReadWrite).await?;

    let set = uid.to_string();
    match edit {
        MailEdit::SetKeywords { add, remove, .. } => {
            if !add.is_empty() {
                connection
                    .uid_store(&set, &flags_item('+', add.iter()))
                    .await?;
            }
            if !remove.is_empty() {
                connection
                    .uid_store(&set, &flags_item('-', remove.iter()))
                    .await?;
            }
            // Both sides empty is a no-op (no STORE issued); the receipt still
            // resolves the pending op.
        }
        MailEdit::MoveTo { destination, .. } => {
            reject_control_chars(destination.as_str())?;
            connection.uid_move(&set, destination.as_str()).await?;
        }
        MailEdit::Delete { .. } => {
            connection
                .uid_store(&set, "+FLAGS.SILENT (\\Deleted)")
                .await?;
            connection.uid_expunge(&set).await?;
        }
    }

    Ok(MailEditReceipt::new(key.clone()))
}

/// Builds a `±FLAGS.SILENT (<flags>)` STORE item from a set of keywords, with `sign`
/// `'+'` (add) or `'-'` (remove). `.SILENT` suppresses the FETCH echo. Caller
/// guarantees the iterator is non-empty.
fn flags_item<'a>(
    sign: char,
    keywords: impl Iterator<Item = &'a engine_core::mail::Keyword>,
) -> String {
    let flags = keywords.map(keyword_to_flag).collect::<Vec<_>>().join(" ");
    format!("{sign}FLAGS.SILENT ({flags})")
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
