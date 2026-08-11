//! Placing a message in a role folder by `APPEND`: the Sent copy of a delivered send and
//! the body of `save_draft`.
//!
//! Split from [`crate::filing`] (which owns the SMTP submission around it) because the
//! placement runs on **two different connections**: the provider's standing session first,
//! and — when that one is dead — a freshly dialed one. Everything here is therefore free
//! functions over a [`Connection<S>`] rather than methods on the provider, so one
//! implementation serves both.

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::MailboxRole,
};
use engine_provider::ProviderResult;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    mail::{mailbox_from_list, message_key},
    transport::{Connection, quote},
};

/// Where a placed copy is filed. One value ties together the SPECIAL-USE role used
/// to resolve the server's real folder, the conventional folder name to fall back
/// to, and the fallback key prefix — so the three can never desync.
#[derive(Clone, Copy)]
pub(crate) enum Filing {
    Sent,
    Drafts,
}

impl Filing {
    /// The RFC 6154 SPECIAL-USE role identifying this folder on the server.
    fn role(self) -> MailboxRole {
        match self {
            Self::Sent => MailboxRole::Sent,
            Self::Drafts => MailboxRole::Drafts,
        }
    }

    /// The conventional folder name to create and use when the server advertises no
    /// folder with [`Self::role`].
    pub(crate) fn default_folder(self) -> &'static str {
        match self {
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
        }
    }

    /// The prefix of the `Message-ID`-derived fallback key (when no UIDPLUS).
    pub(crate) fn key_prefix(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Drafts => "draft",
        }
    }

    /// The IMAP flags to set on the appended copy.
    pub(crate) fn flags(self) -> &'static str {
        match self {
            Self::Sent => "\\Seen",
            Self::Drafts => "\\Draft \\Seen",
        }
    }
}

/// Resolves the real folder for `filing` — the account's folder carrying the matching
/// SPECIAL-USE role, else the conventional name (created if missing) — and `APPEND`s
/// `message` flagged per `filing`, returning the folder used and the UIDPLUS `APPENDUID`
/// if the server supports it.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on a transport failure or
/// a rejected `LIST`/`APPEND`.
pub(crate) async fn append_to_role_folder<S>(
    connection: &mut Connection<S>,
    filing: Filing,
    message: &[u8],
) -> ProviderResult<(String, Option<(u32, u32)>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let folder = resolve_filing_folder(connection, filing).await?;
    let append_uid = connection.append(&folder, filing.flags(), message).await?;
    Ok((folder, append_uid))
}

/// The folder `filing` places into: the account's folder carrying the role, else the
/// conventional name, created if it does not exist (an "already exists" rejection is
/// ignored).
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on a `LIST` failure.
pub(crate) async fn resolve_filing_folder<S>(
    connection: &mut Connection<S>,
    filing: Filing,
) -> ProviderResult<String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Some(name) = resolve_role_folder(connection, filing.role()).await? {
        return Ok(name);
    }
    let name = filing.default_folder().to_owned();
    let _ = connection.create(&name).await;
    Ok(name)
}

/// Finds the account's folder carrying `role` (RFC 6154 SPECIAL-USE) via `LIST`; `None`
/// when the server advertises none.
///
/// Returns the **wire** name — the modified-UTF-7 form the server listed, which is what
/// `SELECT`/`APPEND` take and what a message key embeds — not the decoded display name
/// (`crate::utf7`).
async fn resolve_role_folder<S>(
    connection: &mut Connection<S>,
    role: MailboxRole,
) -> ImapResult<Option<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let rows = connection.list().await?;
    Ok(rows
        .iter()
        .find(|row| {
            mailbox_from_list(row).is_some_and(|mailbox| mailbox.role.as_ref() == Some(&role))
        })
        .map(|row| row.name.clone()))
}

/// Looks for an already-placed copy of `message_id` in `folder`, returning its
/// `(UIDVALIDITY, UID)`.
///
/// This is what makes retrying a failed placement safe. `APPEND` is not idempotent: a first
/// attempt that reached the server and committed but whose response was lost would, on a
/// blind retry, leave the user two copies in Sent. Searching by the `Message-ID` we
/// generated — unique per submission — turns the retry into "place it if it is not already
/// there".
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on a `SELECT`/`SEARCH`
/// failure. A server that cannot search headers is not an error at this layer — it returns
/// no match, and the caller treats "unknown" as "not placed".
pub(crate) async fn find_placed_copy<S>(
    connection: &mut Connection<S>,
    folder: &str,
    message_id: &MessageIdHeader,
) -> ProviderResult<Option<(u32, u32)>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let selected = connection.select(folder).await?;
    // The header value is server-facing input only in the sense that we minted it; quote it
    // anyway, so a `Message-ID` carrying a quote or backslash cannot break the command.
    let criteria = format!("HEADER Message-ID {}", quote(message_id.as_str()));
    let uids = connection.uid_search(&criteria).await?;
    // A duplicate would mean an earlier retry already doubled it; the highest UID is the
    // one a later sync will settle on either way.
    Ok(uids.iter().max().map(|uid| (selected.uid_validity, *uid)))
}

/// The key for a message just placed in `folder`: the real key from UIDPLUS `APPENDUID`,
/// else a `Message-ID`-derived `{prefix}:<id>` key that the next sync of that folder
/// resolves.
pub(crate) fn placed_key(
    folder: &str,
    prefix: &str,
    append_uid: Option<(u32, u32)>,
    message_id: &MessageIdHeader,
) -> ProviderKey {
    match append_uid {
        Some((validity, uid)) => message_key(folder, validity, uid),
        None => ProviderKey::new(format!("{prefix}:{}", message_id.as_str()))
            .expect("a Message-ID-derived placement key is never empty"),
    }
}

#[cfg(test)]
#[path = "place_tests.rs"]
mod tests;
