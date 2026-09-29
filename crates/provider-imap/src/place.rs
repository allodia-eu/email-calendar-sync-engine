//! Placing a message in a role folder by `APPEND`: the Sent copy of a delivered send and
//! the body of `save_draft`.
//!
//! Split from [`crate::filing`] (which owns the SMTP submission around it) because the
//! placement can run on **two different connections**: a pooled one first, and — when that
//! one is dead — another proved alive. Everything here is therefore free
//! functions over a [`Connection<S>`] rather than methods on the provider, so one
//! implementation serves both.

use std::collections::BTreeSet;

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::{Keyword, MailboxRole},
};
use engine_provider::ProviderResult;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    mail::{keyword_to_flag, mailbox_from_list, message_key},
    parse::SelectData,
    transport::Connection,
    transport_command::quote,
};

/// Where a placement put the copy, and which of the keywords asked for it carries.
#[derive(Debug)]
pub(crate) struct Placed {
    /// The folder, decoded.
    pub(crate) folder: String,
    /// `(UIDVALIDITY, UID)` of the copy, when the server said.
    pub(crate) append_uid: Option<(u32, u32)>,
    /// The keywords asked for that the copy carries: all of them where the folder allows
    /// new keywords, none where it does not.
    pub(crate) keywords: BTreeSet<Keyword>,
}

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

    /// [`Self::flags`] plus `keywords`, each flag once.
    fn flags_with(self, keywords: &BTreeSet<Keyword>) -> String {
        let base = self.flags();
        let mut flags = base.to_owned();
        for flag in keywords.iter().map(keyword_to_flag) {
            if !base.split(' ').any(|own| own.eq_ignore_ascii_case(&flag)) {
                flags.push(' ');
                flags.push_str(&flag);
            }
        }
        flags
    }
}

/// The keywords of `asked` that `selected` lets a client store: all of them when its
/// `PERMANENTFLAGS` carries `\*`, none otherwise.
///
/// Without `\*` a server accepts the `APPEND` flags with a plain `OK` and keeps no new
/// keyword (RFC 9051 §7.1), so asking would read as kept and be gone on the next `FETCH`.
/// A `PERMANENTFLAGS` that names a keyword individually is read as not allowing it: the
/// conservative reading, since the parse records only `\*`.
fn keepable(selected: &SelectData, asked: &BTreeSet<Keyword>) -> BTreeSet<Keyword> {
    if selected.permanent_flags_allow_new {
        asked.clone()
    } else {
        BTreeSet::new()
    }
}

/// Resolves the real folder for `filing` — the account's folder carrying the matching
/// SPECIAL-USE role, else the conventional name (created if missing) — and `APPEND`s
/// `message` flagged per `filing` plus whichever of `keywords` the folder keeps.
///
/// Asking for keywords costs a `SELECT`, the only way to read `PERMANENTFLAGS`; asking for
/// none costs nothing. A `SELECT` that fails leaves the keywords out rather than failing the
/// placement: they are never worth the copy.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on a transport failure or
/// a rejected `LIST`/`APPEND`.
pub(crate) async fn append_to_role_folder<S>(
    connection: &mut Connection<S>,
    filing: Filing,
    message: &[u8],
    keywords: &BTreeSet<Keyword>,
) -> ProviderResult<Placed>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let folder = resolve_filing_folder(connection, filing).await?;
    let keywords = if keywords.is_empty() {
        BTreeSet::new()
    } else {
        connection
            .select(&folder)
            .await
            .map(|selected| keepable(&selected, keywords))
            .unwrap_or_default()
    };
    let append_uid = connection
        .append(&folder, &filing.flags_with(&keywords), message)
        .await?;
    Ok(Placed {
        folder,
        append_uid,
        keywords,
    })
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
/// Returns the **decoded** name, the same form a [`Mailbox`](engine_core::mail::Mailbox)
/// id carries, so a message key built from it matches the one the folder list produced.
/// [`crate::transport`] re-encodes it for the `SELECT`/`APPEND` that follow.
async fn resolve_role_folder<S>(
    connection: &mut Connection<S>,
    role: MailboxRole,
) -> ImapResult<Option<String>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let modified_utf7 = connection.names_are_modified_utf7();
    let rows = connection.list().await?;
    Ok(rows
        .iter()
        .find(|row| {
            mailbox_from_list(row, modified_utf7)
                .is_some_and(|mailbox| mailbox.role.as_ref() == Some(&role))
        })
        .map(|row| {
            if modified_utf7 {
                crate::utf7::decode(&row.name)
            } else {
                row.name.clone()
            }
        }))
}

/// Places `message` for `filing` **only if a copy of `message_id` is not already there**.
///
/// The idempotent form of [`append_to_role_folder`], for every attempt after the first: a
/// retry cannot know whether the attempt it is repeating reached the server, and `APPEND`
/// would happily make a second copy. The first attempt of a send skips the probe, since the
/// copy definitionally is not there yet and a `SELECT` + `SEARCH` on every send is not free.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on a transport failure or
/// a rejected `LIST`/`SELECT`/`SEARCH`/`APPEND`.
///
/// A copy already there is given `keywords` too: the attempt that placed it may not have
/// set them, and `+FLAGS` is idempotent. A `STORE` that fails leaves them out rather than
/// failing the placement.
pub(crate) async fn place_if_absent<S>(
    connection: &mut Connection<S>,
    filing: Filing,
    message_id: &MessageIdHeader,
    message: &[u8],
    keywords: &BTreeSet<Keyword>,
) -> ProviderResult<Placed>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let folder = resolve_filing_folder(connection, filing).await?;
    let (selected, existing) = probe_placed_copy(connection, &folder, message_id).await?;
    let mut keywords = keepable(&selected, keywords);
    if let Some((_, uid)) = existing {
        if !keywords.is_empty() {
            let flags: Vec<String> = keywords.iter().map(keyword_to_flag).collect();
            let item = format!("+FLAGS.SILENT ({})", flags.join(" "));
            if connection.uid_store(&uid.to_string(), &item).await.is_err() {
                keywords.clear();
            }
        }
        return Ok(Placed {
            folder,
            append_uid: existing,
            keywords,
        });
    }
    let append_uid = connection
        .append(&folder, &filing.flags_with(&keywords), message)
        .await?;
    Ok(Placed {
        folder,
        append_uid,
        keywords,
    })
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
    Ok(probe_placed_copy(connection, folder, message_id).await?.1)
}

/// [`find_placed_copy`], also returning what the `SELECT` said about the folder.
async fn probe_placed_copy<S>(
    connection: &mut Connection<S>,
    folder: &str,
    message_id: &MessageIdHeader,
) -> ProviderResult<(SelectData, Option<(u32, u32)>)>
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
    let found = uids.iter().max().map(|uid| (selected.uid_validity, *uid));
    Ok((selected, found))
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

#[cfg(test)]
#[path = "place_keyword_tests.rs"]
mod keyword_tests;
