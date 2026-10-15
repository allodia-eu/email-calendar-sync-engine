//! Fetching the list-level metadata of messages: two commands, because one item is costly.
//!
//! `BODYSTRUCTURE` answers one question here, whether a message carries a download, and some
//! servers build it by reading the whole message: Yahoo spends about 25 ms a message on it,
//! even on a single-part one, against under 3 ms for everything else in [`FETCH_ITEMS`]
//! together. A message whose top-level headers say it is one text body
//! ([`engine_mime::is_single_text_body`]) cannot carry one, so the first command asks for
//! those headers, and the second ([`STRUCTURE_ITEMS`]) asks for `BODYSTRUCTURE` only for the
//! rows the headers left open (`has_attachment` of `None`).
//!
//! That second command is shared out over spare connections when the pool has some
//! ([`spare_helpers`], [`settle_across`]): Yahoo answers it about three times as fast over four
//! sessions as over one. A spare connection is one nobody is waiting for, so the sharing never
//! holds up another folder's pass.

use std::sync::Arc;

use futures_util::future::{join, join_all};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult, parse::FetchRow, pool::ImapPool, pool_lease::PooledConnection,
    sync::uid_set_spec, transport::Connection,
};

/// The most spare connections one pass borrows to share a structure fetch. Yahoo's throughput
/// stops growing at three or four sessions in all.
const MAX_HELPERS: usize = 3;

/// The fewest rows worth a connection of their own: below this a share costs more in its
/// round trip than it saves.
const MIN_SHARE: usize = 20;

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

/// Spare connections, at most one for every [`MIN_SHARE`] of `open` rows, each with `mailbox`
/// open at `uid_validity`. Only what the pool has free right now; none for a pass without a pool.
pub(crate) async fn spare_helpers<S>(
    pool: Option<&Arc<ImapPool<S>>>,
    mailbox: &str,
    uid_validity: u32,
    open: usize,
) -> Vec<PooledConnection<S>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    let mut helpers = Vec::new();
    let Some(pool) = pool else {
        return helpers;
    };
    let wanted = (open / MIN_SHARE).saturating_sub(1).min(MAX_HELPERS);
    while helpers.len() < wanted {
        let Some(mut helper) = pool.try_acquire_for(mailbox).await else {
            break;
        };
        if helper.open_validity(mailbox) != Some(uid_validity) {
            match helper.examine(mailbox).await {
                Ok(select) if select.uid_validity == uid_validity => {}
                Ok(_) => break,
                Err(_) => {
                    helper.discard();
                    break;
                }
            }
        }
        helpers.push(helper);
    }
    helpers
}

/// Settles `open` with one [`STRUCTURE_ITEMS`] fetch per connection, `conn` and each of
/// `helpers` asking for an equal share at once. A helper that fails is discarded and its share
/// asked again on `conn`. Returns the settled rows; a row the server returned no structure for
/// stays in `open`.
pub(crate) async fn settle_across<S>(
    conn: &mut Connection<S>,
    helpers: Vec<PooledConnection<S>>,
    open: &mut Vec<FetchRow>,
) -> ImapResult<Vec<FetchRow>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut uids: Vec<u32> = open.iter().map(|row| row.uid).collect();
    uids.sort_unstable();
    uids.dedup();
    let per_set = conn.negotiated.within_message_limit(STRUCTURE_BATCH);
    let share = uids.len().div_ceil(helpers.len() + 1).max(1);
    let mut shares = uids.chunks(share);
    let own = shares.next().unwrap_or_default();
    let others = helpers
        .into_iter()
        .zip(shares)
        .map(|(mut helper, share)| async move {
            let answer = fetch_structures(&mut helper, share, per_set).await;
            (helper, share, answer)
        });
    let (own_answer, others) = join(fetch_structures(conn, own, per_set), join_all(others)).await;
    let mut answers = own_answer?;
    let mut retry: Vec<u32> = Vec::new();
    for (helper, share, answer) in others {
        if let Ok(rows) = answer {
            answers.extend(rows);
        } else {
            helper.discard();
            retry.extend_from_slice(share);
        }
    }
    answers.extend(fetch_structures(conn, &retry, per_set).await?);
    Ok(answers
        .iter()
        .filter_map(|answer| take_settled(open, answer))
        .collect())
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

/// The structures of `uids`, one [`STRUCTURE_ITEMS`] command per [`structure_sets`] set.
async fn fetch_structures<S>(
    conn: &mut Connection<S>,
    uids: &[u32],
    per_set: usize,
) -> ImapResult<Vec<FetchRow>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut answers = Vec::new();
    for set in structure_sets(uids.to_vec(), per_set) {
        answers.extend(conn.uid_fetch(&set, STRUCTURE_ITEMS).await?);
    }
    Ok(answers)
}

#[cfg(test)]
#[path = "metadata_fetch_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "metadata_spread_tests.rs"]
mod spread_tests;
