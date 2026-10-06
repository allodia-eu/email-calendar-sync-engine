//! Fetching the list-level metadata of messages: two commands, because one item is costly.
//!
//! `BODYSTRUCTURE` answers one question here, whether a message carries a download, and some
//! servers build it by reading the whole message: Yahoo spends about 25 ms a message on it,
//! even on a single-part one, against under 3 ms for everything else in [`FETCH_ITEMS`]
//! together. A message whose top-level headers say it is one text body
//! ([`engine_mime::is_single_text_body`]) cannot carry one, so the first command asks for
//! those headers, and the second ([`STRUCTURE_ITEMS`]) asks for `BODYSTRUCTURE` only for the
//! rows the headers left open (`has_attachment` of `None`).

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{error::ImapResult, parse::FetchRow, sync::uid_set_spec, transport::Connection};

/// The metadata `FETCH` items, all peek-safe (none sets `\Seen`).
///
/// The header section carries `References`, which `ENVELOPE` omits (RFC 9051 §7.5.2) and
/// local threading needs, and the two headers that decide whether `BODYSTRUCTURE` is needed.
/// The peek form is required so the read does not set `\Seen`; the server echoes it back
/// without `.PEEK`.
pub(crate) const FETCH_ITEMS: &str = concat!(
    "UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE ",
    "BODY.PEEK[HEADER.FIELDS (REFERENCES CONTENT-TYPE CONTENT-DISPOSITION)]"
);

/// The second command's items, for the rows [`FETCH_ITEMS`] left open.
pub(crate) const STRUCTURE_ITEMS: &str = "UID BODYSTRUCTURE";

/// The most UIDs one [`STRUCTURE_ITEMS`] command names: about 5.5 KB of set at worst, under the
/// 8192-octet command line RFC 7162 §4 asks a client to stay within.
pub(crate) const STRUCTURE_BATCH: usize = 500;

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `UID FETCH <set>` of [`FETCH_ITEMS`], with every row's attachment flag settled.
    pub(crate) async fn uid_fetch_metadata(&mut self, set: &str) -> ImapResult<Vec<FetchRow>> {
        let mut rows = self.uid_fetch(set, FETCH_ITEMS).await?;
        self.settle_attachments(&mut rows).await?;
        Ok(rows)
    }

    /// Settles the attachment flag of each row that [`needs_structure`], with a
    /// `UID FETCH` of [`STRUCTURE_ITEMS`] per [`structure_sets`] group of those rows. A row
    /// the server returns no structure for (expunged meanwhile) keeps `None`.
    pub(crate) async fn settle_attachments(&mut self, rows: &mut [FetchRow]) -> ImapResult<()> {
        let open: Vec<u32> = rows
            .iter()
            .filter(|row| needs_structure(row))
            .map(|row| row.uid)
            .collect();
        for set in structure_sets(open, self.negotiated.within_message_limit(STRUCTURE_BATCH)) {
            for answer in self.uid_fetch(&set, STRUCTURE_ITEMS).await? {
                let Some(found) = answer.has_attachment else {
                    continue;
                };
                if let Some(row) = rows
                    .iter_mut()
                    .find(|row| row.uid == answer.uid && needs_structure(row))
                {
                    row.has_attachment = Some(found);
                }
            }
        }
        Ok(())
    }
}

/// Whether `row` is a solicited metadata row whose headers left the attachment flag open.
/// A row without an envelope is an unsolicited flag update, not a message.
pub(crate) fn needs_structure(row: &FetchRow) -> bool {
    row.has_attachment.is_none() && row.envelope.is_some()
}

/// The UID sets naming `uids`, at most `per_set` UIDs each, so neither the server's message
/// limit nor its command-line length is exceeded: the open rows are scattered, so a set names
/// most of them one by one.
pub(crate) fn structure_sets(mut uids: Vec<u32>, per_set: usize) -> Vec<String> {
    uids.sort_unstable();
    uids.dedup();
    uids.chunks(per_set.max(1)).map(uid_set_spec).collect()
}

/// Takes the row in `open` that `answer`, a [`STRUCTURE_ITEMS`] row, settles, with its flag
/// set. `None` for a row that carries no structure (an unsolicited flag update) or names no
/// open row.
pub(crate) fn take_settled(open: &mut Vec<FetchRow>, answer: &FetchRow) -> Option<FetchRow> {
    let has_attachment = answer.has_attachment?;
    let index = open.iter().position(|row| row.uid == answer.uid)?;
    let mut row = open.swap_remove(index);
    row.has_attachment = Some(has_attachment);
    Some(row)
}

#[cfg(test)]
#[path = "metadata_fetch_tests.rs"]
mod tests;
