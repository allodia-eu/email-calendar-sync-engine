//! Gmail's mail as one store, read over IMAP with Gmail's `X-GM-EXT-1` extension.
//!
//! Gmail files a message under labels, not in a folder, and shows each label to IMAP as a
//! folder holding a copy. Synced folder by folder, a message with three labels is downloaded
//! three times and stored as three objects. Gmail's All Mail (`\All`) holds every message
//! once, and `X-GM-EXT-1` gives each message the facts that make that copy enough: its Gmail
//! id (`X-GM-MSGID`, the same in every folder; in hex it is the Gmail API's id), its
//! conversation (`X-GM-THRID`) and its labels (`X-GM-LABELS`). All Mail leaves out Spam and
//! Trash, so those two are read beside it, and a message moving between the three keeps its
//! Gmail id under a new UID.
//!
//! This module reads; what a label or a flag *means* is the caller's (the Gmail adapter, which
//! names labels by the API's ids). It is offered only on a session that advertises
//! `X-GM-EXT-1` ([`ImapAccount::gmail`](crate::ImapAccount::gmail)).

use std::sync::Arc;

use engine_core::{
    ids::MailboxId,
    mail::{MailboxRole, Message},
    raw::RawMime,
};
use engine_provider::{ProviderError, ProviderResult};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::OnceCell,
};

use crate::{gmail_read as read, mail::mailbox_from_list, pool::ImapPool, sync::uid_set_spec};

/// The bodies [`GmailMail::sources`] hands on, each with its Gmail id, as they arrive.
pub type GmailSources<'a> = core::pin::Pin<
    Box<dyn futures_util::Stream<Item = ProviderResult<(u64, RawMime)>> + Send + 'a>,
>;

/// One of the three folders that together hold every message of a Gmail account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GmailFolder {
    /// All Mail (`\All`): every message but spam and trash.
    All,
    /// Spam (`\Junk`).
    Spam,
    /// Trash (`\Trash`).
    Trash,
}

impl GmailFolder {
    /// The three, in the order a pass reads them.
    pub const ALL: [Self; 3] = [Self::All, Self::Spam, Self::Trash];
}

/// A folder's counters as `SELECT (CONDSTORE)` reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmailFolderState {
    /// `UIDVALIDITY`: a change renumbers every UID in the folder.
    pub uid_validity: u32,
    /// `UIDNEXT`: every UID from here on arrived after this read.
    pub uid_next: u32,
    /// `HIGHESTMODSEQ`: every change after this read has a higher mod-sequence.
    pub highest_modseq: u64,
    /// `EXISTS`: how many messages the folder holds.
    pub exists: u32,
}

/// Where one message is filed and how it is marked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailState {
    /// Its UID in the folder it was read from.
    pub uid: u32,
    /// `X-GM-MSGID`.
    pub gmail_id: u64,
    /// `X-GM-THRID`.
    pub thread_id: u64,
    /// `X-GM-LABELS`, decoded: a system label as Gmail names it (`\Inbox`, `\Sent`,
    /// `\Important`), any other by its name.
    pub labels: Vec<String>,
    /// The IMAP flags (`\Seen`, `\Flagged`, `\Draft`, …).
    pub flags: Vec<String>,
    /// Its mod-sequence, when the read asked for changes.
    pub modseq: Option<u64>,
}

/// A message's state and its metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct GmailMessage {
    /// Where it is filed and how it is marked.
    pub state: GmailState,
    /// The envelope, size, dates, references and attachment flag. Its id and memberships
    /// are IMAP's (the folder and UID it was read from), for the caller to replace.
    pub message: Message,
    /// The `Date` header as the message gave it, for the caller to read as its sent time.
    pub date: Option<String>,
}

/// Which messages of a folder a [`GmailMail::search`] asks for; every criterion given must
/// hold, and none given asks for all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GmailQuery {
    /// Only these UIDs.
    pub uids: Option<Vec<u32>>,
    /// Only UIDs from this one on.
    pub from_uid: Option<u32>,
    /// Only messages that arrived on or after this day (`SINCE`).
    pub since: Option<time::Date>,
    /// Only messages Gmail's own search matches (`X-GM-RAW`), e.g. `category:social`.
    pub raw: Option<String>,
}

/// The folder names an account gives the three.
#[derive(Debug, Clone)]
struct Folders {
    all: MailboxId,
    spam: Option<MailboxId>,
    trash: Option<MailboxId>,
}

/// A Gmail account's mail read as one store; see the module docs. Cheap to clone, and every
/// call borrows a connection from the account's pool.
pub struct GmailMail<S> {
    pool: Arc<ImapPool<S>>,
    folders: Arc<OnceCell<Folders>>,
}

impl<S> core::fmt::Debug for GmailMail<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GmailMail")
            .field("pool", &self.pool)
            .field("folders", &self.folders.get())
            .finish()
    }
}

impl<S> Clone for GmailMail<S> {
    fn clone(&self) -> Self {
        Self {
            pool: Arc::clone(&self.pool),
            folders: Arc::clone(&self.folders),
        }
    }
}

impl<S> GmailMail<S> {
    pub(crate) fn new(pool: Arc<ImapPool<S>>) -> Self {
        Self {
            pool,
            folders: Arc::new(OnceCell::new()),
        }
    }
}

impl<S> GmailMail<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    /// Opens `folder`, reporting its counters; `None` when the account shows no such folder
    /// to IMAP (a user can hide Spam and Trash in Gmail's settings).
    ///
    /// # Errors
    ///
    /// [`ProviderError::invalid_state`] when the account shows no All Mail to IMAP, which
    /// a user can also hide; anything the server or the network answered.
    pub async fn open(&self, folder: GmailFolder) -> ProviderResult<Option<GmailFolderState>> {
        let Some(name) = self.name(folder).await? else {
            return Ok(None);
        };
        let mut connection = self.pool.acquire_for(name.as_str()).await?;
        let result = read::open(&mut connection, name.as_str())
            .await
            .map_err(Into::into);
        connection.settle(result).map(Some)
    }

    /// The UIDs in `folder` that `query` matches, ascending.
    ///
    /// # Errors
    ///
    /// [`ProviderError::conflict`] when the folder is no longer at `validity`; anything the
    /// server or the network answered.
    pub async fn search(
        &self,
        folder: GmailFolder,
        validity: u32,
        query: &GmailQuery,
    ) -> ProviderResult<Vec<u32>> {
        self.on(folder, validity, async |connection| {
            Ok(read::search(connection, query).await?)
        })
        .await
    }

    /// The metadata of `uids` in `folder`, newest first; a UID gone meanwhile is left out.
    ///
    /// # Errors
    ///
    /// As [`search`](Self::search).
    pub async fn messages(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>> {
        let name = self.required(folder).await?;
        self.on(folder, validity, async |connection| {
            Ok(read::messages(connection, &name, validity, uids).await?)
        })
        .await
    }

    /// The state of every message in `folder` below `below_uid` changed after `modseq`.
    ///
    /// # Errors
    ///
    /// As [`search`](Self::search).
    pub async fn changed(
        &self,
        folder: GmailFolder,
        validity: u32,
        below_uid: u32,
        modseq: u64,
    ) -> ProviderResult<Vec<GmailState>> {
        self.on(folder, validity, async |connection| {
            Ok(read::changed(connection, below_uid, modseq).await?)
        })
        .await
    }

    /// The Gmail id of each of `uids` in `folder`, as `(uid, id)`.
    ///
    /// # Errors
    ///
    /// As [`search`](Self::search).
    pub async fn ids(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<(u32, u64)>> {
        self.on(folder, validity, async |connection| {
            Ok(read::ids(connection, uids).await?)
        })
        .await
    }

    /// The raw source of each message named by its Gmail id, looked for in All Mail, then
    /// Trash, then Spam, each handed on as it comes off the wire, so a batch holds one body in
    /// memory at a time (as `crate::fetch_batch` does). An id none of them holds is left out.
    /// The stream ends after the first failure, which it yields.
    pub fn sources<'a>(&'a self, gmail_ids: &'a [u64]) -> GmailSources<'a> {
        Box::pin(async_stream::stream! {
            let mut wanted = gmail_ids.to_vec();
            for folder in [GmailFolder::All, GmailFolder::Trash, GmailFolder::Spam] {
                if wanted.is_empty() {
                    break;
                }
                let name = match self.name(folder).await {
                    Ok(Some(name)) => name,
                    Ok(None) => continue,
                    Err(err) => {
                        yield Err(err);
                        return;
                    }
                };
                let pending = wanted.clone();
                for batch in pending.chunks(read::SOURCE_BATCH) {
                    let mut connection = match self.pool.acquire_for(name.as_str()).await {
                        Ok(connection) => connection,
                        Err(err) => {
                            yield Err(err.into());
                            return;
                        }
                    };
                    let located = async {
                        connection.examine(name.as_str()).await?;
                        read::located(&mut connection, batch).await
                    }
                    .await;
                    let located = match located {
                        Ok(located) if located.is_empty() => continue,
                        Ok(located) => located,
                        Err(err) => {
                            connection.discard();
                            yield Err(err.into());
                            return;
                        }
                    };
                    let mut uids: Vec<u32> = located.iter().map(|(uid, _)| *uid).collect();
                    uids.sort_unstable();
                    let started = connection
                        .uid_fetch_stream_start(&uid_set_spec(&uids), "BODY.PEEK[]")
                        .await;
                    if let Err(err) = started {
                        connection.discard();
                        yield Err(err.into());
                        return;
                    }
                    loop {
                        match connection.next_fetch_body().await {
                            Ok(Some((uid, bytes))) => {
                                let Some(&(_, id)) = located.iter().find(|(known, _)| *known == uid)
                                else {
                                    continue;
                                };
                                wanted.retain(|want| *want != id);
                                yield Ok((id, RawMime::new(bytes)));
                            }
                            Ok(None) => break,
                            Err(err) => {
                                connection.discard();
                                yield Err(err.into());
                                return;
                            }
                        }
                    }
                }
            }
        })
    }

    /// Runs `work` on a connection with `folder` open at `validity`.
    async fn on<T>(
        &self,
        folder: GmailFolder,
        validity: u32,
        work: impl AsyncFnOnce(&mut crate::transport::Connection<S>) -> ProviderResult<T>,
    ) -> ProviderResult<T> {
        let name = self.required(folder).await?;
        let mut connection = self.pool.acquire_for(name.as_str()).await?;
        let result = async {
            read::ensure_open(&mut connection, name.as_str(), validity).await?;
            work(&mut connection).await
        }
        .await;
        connection.settle(result)
    }

    async fn required(&self, folder: GmailFolder) -> ProviderResult<MailboxId> {
        self.name(folder).await?.ok_or_else(|| {
            ProviderError::invalid_state(format!("the account shows no {folder:?} folder to IMAP"))
        })
    }

    async fn name(&self, folder: GmailFolder) -> ProviderResult<Option<MailboxId>> {
        let folders = self.folders.get_or_try_init(|| self.list()).await?;
        Ok(match folder {
            GmailFolder::All => Some(folders.all.clone()),
            GmailFolder::Spam => folders.spam.clone(),
            GmailFolder::Trash => folders.trash.clone(),
        })
    }

    /// Finds the three by their special-use roles.
    async fn list(&self) -> ProviderResult<Folders> {
        let mut connection = self.pool.acquire().await?;
        let utf7 = connection.negotiated.names_are_modified_utf7();
        let result = connection.list("*").await.map_err(ProviderError::from);
        let rows = connection.settle(result)?;
        let find = |role: MailboxRole| {
            rows.iter()
                .filter_map(|row| mailbox_from_list(row, utf7))
                .find(|mailbox| mailbox.role.as_ref() == Some(&role))
                .map(|mailbox| mailbox.id)
        };
        let all = find(MailboxRole::All).ok_or_else(|| {
            ProviderError::invalid_state("the account shows no All Mail folder to IMAP")
        })?;
        Ok(Folders {
            all,
            spam: find(MailboxRole::Junk),
            trash: find(MailboxRole::Trash),
        })
    }
}

#[cfg(test)]
#[path = "gmail_tests.rs"]
mod tests;
