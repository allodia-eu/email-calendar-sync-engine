//! Storing one message in a folder with the metadata asked for, and saying what was kept: the
//! one routine behind a Sent copy, a draft and an import.
//!
//! Every one of them is an `APPEND` that asks the folder to keep some keywords, and a folder
//! that cannot keep one answers `OK` and drops it. So which keywords go on the `APPEND`, and
//! which the receipt reports kept, is decided here, once, by the rule in [`PermanentFlags`].

use std::collections::{BTreeMap, BTreeSet};

use engine_core::mail::{Keyword, MessageMetadata};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    mail::keyword_to_flag,
    permanent::PermanentFlags,
    transport::Connection,
    transport_append::{AppendFailure, internal_date_instant},
};

/// What a stored message got: its `(UIDVALIDITY, UID)` where the server said (UIDPLUS), and the
/// metadata it carries of what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stored {
    /// `(UIDVALIDITY, UID)` of the stored message, when the server said.
    pub(crate) append_uid: Option<(u32, u32)>,
    /// Of the metadata asked for, what the stored message carries.
    pub(crate) kept: MessageMetadata,
}

/// What each folder written to keeps, read once per folder.
///
/// A folder is opened (`SELECT`) only when a keyword is asked for it, since that is the only
/// way to read `PERMANENTFLAGS`; storing a message asking for none costs nothing extra. A folder
/// that refuses to be opened (a tagged `NO` or `BAD`: no read right, say) still takes the
/// message: it is stored without the keywords, which is what the receipt then says. Keywords
/// are never worth the message.
#[derive(Debug, Default)]
pub(crate) struct FolderKeywords {
    by_folder: BTreeMap<String, PermanentFlags>,
}

impl FolderKeywords {
    /// Records what `folder` said when it was opened elsewhere, so it is not opened again.
    pub(crate) fn record(&mut self, folder: &str, permanent: PermanentFlags) {
        self.by_folder.insert(folder.to_owned(), permanent);
    }

    /// The keywords of `asked` that `folder` keeps, opening it the first time a keyword is
    /// asked for it.
    ///
    /// # Errors
    ///
    /// The connection's failure when the `SELECT` got no answer; a refusal is not an error.
    pub(crate) async fn keepable<S>(
        &mut self,
        connection: &mut Connection<S>,
        folder: &str,
        asked: &BTreeSet<Keyword>,
    ) -> ImapResult<BTreeSet<Keyword>>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        if asked.is_empty() {
            return Ok(BTreeSet::new());
        }
        if let Some(permanent) = self.by_folder.get(folder) {
            return Ok(permanent.keepable(asked));
        }
        let permanent = match connection.select(folder).await {
            Ok(selected) => selected.permanent,
            Err(ImapError::No(_) | ImapError::Bad(_)) => PermanentFlags::none(),
            Err(lost) => return Err(lost),
        };
        let keepable = permanent.keepable(asked);
        self.record(folder, permanent);
        Ok(keepable)
    }
}

/// `APPEND`s `message` to `folder` with `role_flags` (a draft's `\Draft`, a Sent copy's
/// `\Seen`) and whatever of `asked` the folder keeps, answering what was kept.
///
/// The arrival time asked for goes as the `INTERNALDATE`, in the whole seconds one holds, and
/// is kept as sent; a time it cannot hold (before year 0) is left for the server to stamp. A
/// role flag is the filing's own, not something asked, so it is never reported kept unless it
/// was also asked for.
///
/// # Errors
///
/// [`AppendFailure`]: whether the message may have been stored decides what a caller may do
/// next, so it is never folded into a plain error here.
pub(crate) async fn store_message<S>(
    connection: &mut Connection<S>,
    folders: &mut FolderKeywords,
    folder: &str,
    role_flags: &[&str],
    asked: &MessageMetadata,
    message: &[u8],
) -> Result<Stored, AppendFailure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let keywords = folders
        .keepable(connection, folder, &asked.keywords)
        .await
        .map_err(AppendFailure::NotStored)?;
    let received_at = asked.received_at.and_then(internal_date_instant);
    let append_uid = connection
        .append(
            folder,
            &flag_list(role_flags, &keywords),
            received_at,
            message,
        )
        .await?;
    let mut kept = MessageMetadata::default().with_keywords(keywords);
    if let Some(at) = received_at {
        kept = kept.with_received_at(at);
    }
    Ok(Stored { append_uid, kept })
}

/// The `APPEND` flag list: `role_flags`, then each of `keywords` as its flag, each flag once
/// whatever its case.
pub(crate) fn flag_list(role_flags: &[&str], keywords: &BTreeSet<Keyword>) -> String {
    let mut flags: Vec<String> = role_flags.iter().map(|&flag| flag.to_owned()).collect();
    for flag in keywords.iter().map(keyword_to_flag) {
        if !flags.iter().any(|own| own.eq_ignore_ascii_case(&flag)) {
            flags.push(flag);
        }
    }
    flags.join(" ")
}

#[cfg(test)]
#[path = "store_message_tests.rs"]
mod tests;
