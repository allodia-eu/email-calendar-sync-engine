//! Storing a draft on the server, and removing one.
//!
//! `APPEND`s the assembled message to the account's `\Drafts` folder, then removes the
//! copy it supersedes (`UID STORE +FLAGS (\Deleted)` + `UID EXPUNGE`, RFC 4315). Both
//! halves are stream-generic and live here rather than on the provider, so the ordering
//! rule below is unit-testable against a mock stream.
//!
//! **The append comes first, and the removal may fail without failing the save.** IMAP
//! messages are immutable, so "replace" is two commands with no transaction around
//! them, and the order decides which way a partial failure falls: append-then-remove
//! leaves the user two drafts, remove-then-append can leave them none. For a message
//! nobody has sent, a duplicate is recoverable and a loss is not. A removal that fails
//! therefore returns the new key rather than an error: the alternative is an op the
//! caller retries, appending a further copy each time.

use engine_core::ids::{MessageIdHeader, ProviderKey};
use engine_provider::{Draft, ProviderResult};
use engine_rfc5322::assemble_filed_message;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    place::{Filing, find_placed_copy, placed_key, resolve_filing_folder},
    target::{Access, select_target},
    transport::Connection,
};

/// Stores `draft` in the account's Drafts folder, removing the copy named by
/// `replacing`, and returns the new copy's key.
///
/// The key is the real one from UIDPLUS `APPENDUID` where the server offers it, else a
/// `Message-ID`-derived key the next Drafts sync resolves. A saved draft keeps its `Bcc`
/// header, so resuming it restores every recipient; it is appended locally and never
/// transmitted.
///
/// # Errors
///
/// A classified [`ProviderError`] if the `APPEND` itself failed. A failure to remove the
/// superseded copy is **not** one (see the module note).
pub(crate) async fn put_draft<S>(
    connection: &mut Connection<S>,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
) -> ProviderResult<ProviderKey>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let message = assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    let folder = resolve_filing_folder(connection, Filing::Drafts).await?;
    let append_uid = connection
        .append(&folder, Filing::Drafts.flags(), &message)
        .await?;

    if let Some(superseded) = replacing {
        // Best effort by design: the new copy is stored, and reporting this as a failure
        // would have the caller append another.
        let _ = remove(connection, superseded).await;
    }
    Ok(placed_key(
        &folder,
        Filing::Drafts.key_prefix(),
        append_uid,
        &draft.message_id,
    ))
}

/// Removes a stored draft, reporting an absent one as done.
///
/// # Errors
///
/// A classified [`ProviderError`] on a transport failure, or
/// [`ProviderError::conflict`] if the target folder's `UIDVALIDITY` has moved: the UID
/// now names a different message, and deleting it would delete someone else's mail.
pub(crate) async fn delete_draft<S>(
    connection: &mut Connection<S>,
    draft: &ProviderKey,
) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    remove(connection, draft).await
}

/// The removal both paths share: address the copy, flag it `\Deleted`, expunge it.
async fn remove<S>(connection: &mut Connection<S>, draft: &ProviderKey) -> ProviderResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let uid = match unplaced_message_id(draft) {
        // A key minted without UIDPLUS names the draft by the `Message-ID` we wrote, so
        // it has to be searched for before it can be addressed.
        Some(message_id) => {
            let folder = resolve_filing_folder(connection, Filing::Drafts).await?;
            match find_placed_copy(connection, &folder, &message_id).await? {
                Some((_validity, uid)) => uid,
                // Nothing there under that `Message-ID`: the state the caller asked for.
                None => return Ok(()),
            }
        }
        None => select_target(connection, draft, Access::ReadWrite).await?.1,
    };

    let set = uid.to_string();
    // A `UID STORE` naming a UID the folder no longer holds is a no-op, not an error, so
    // a repeated delete settles rather than parking the op that drives it.
    connection
        .uid_store(&set, "+FLAGS.SILENT (\\Deleted)")
        .await?;
    connection.uid_expunge(&set).await?;
    Ok(())
}

/// The `Message-ID` inside a `draft:<id>` key, i.e. one minted for a server without
/// UIDPLUS. `None` for an ordinary `imap:v<validity>:u<uid>@<folder>` key, which
/// addresses its message directly.
fn unplaced_message_id(key: &ProviderKey) -> Option<MessageIdHeader> {
    let prefix = format!("{}:", Filing::Drafts.key_prefix());
    let id = key.as_str().strip_prefix(&prefix)?;
    MessageIdHeader::new(id).ok()
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
mod tests;
