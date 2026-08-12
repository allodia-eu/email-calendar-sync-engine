//! Per-mailbox unread counts: `LIST-STATUS` (RFC 5819) where the server has it,
//! else one `STATUS … (UNSEEN)` per mailbox (RFC 9051 §6.3.11).
//!
//! IMAP puts the unread count nowhere in `LIST` — the folder list and the counts are
//! two different questions, and the naive shape asks the second one once per folder.
//! On an account with fifty folders that is fifty sequential round trips **per folder-list
//! sync**, which is why the extension exists: `LIST "" "*" RETURN (STATUS (UNSEEN))`
//! interleaves a `* STATUS` line with each `* LIST` line and answers both in one.
//!
//! The fallback is kept because the extension is optional — `providers.md`: "optional
//! capabilities, not assumptions" — and a server without it still owes the user a
//! folder badge. Its cost is bounded by [`MAX_STATUS_PROBES`]: past that the remaining
//! folders report no count rather than turning a sync into a minutes-long stall.
//!
//! `UNSEEN` in a `STATUS` response is a **count** of messages without `\Seen` (RFC 9051
//! §7.3.5), not the sequence number of the first unseen message that the same word means
//! in a `SELECT` response code. Only the former is read here.

use std::collections::HashMap;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::{ImapError, ImapResult},
    parse::ListRow,
    tokenize::{Item, items_of},
    transport::Connection,
    transport_command::{list_command, quote},
};

/// How many mailboxes the per-mailbox fallback will probe in one pass. A server
/// without `LIST-STATUS` pays one round trip each, so this bounds the tail: the
/// folders past it simply report no count (which renders as no badge) instead of
/// holding the folder-list sync open indefinitely. Sized well above a normal
/// account's folder count so that in practice it caps only pathological ones.
const MAX_STATUS_PROBES: usize = 100;

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `LIST "" "*" RETURN (SPECIAL-USE STATUS (UNSEEN))` — every mailbox, its role and
    /// its unread count in one round trip. Only valid when the server advertised
    /// `LIST-STATUS`; the `SPECIAL-USE` option rides along only where that too was
    /// advertised.
    ///
    /// Both options are needed, because an **extended** `LIST` returns exactly the
    /// extended data its options name (RFC 5258 §3): asking only for the counts is how a
    /// folder list ends up with every badge and no roles, on the same server whose plain
    /// `LIST` volunteers them.
    ///
    /// Returns the rows and the counts keyed by mailbox name. A server may answer a
    /// `LIST` row with no `STATUS` line (it does that for `\Noselect` containers, which
    /// hold no messages to count), so the map is deliberately sparse rather than one
    /// entry per row.
    pub(crate) async fn list_with_unseen(
        &mut self,
    ) -> ImapResult<(Vec<ListRow>, HashMap<String, u32>)> {
        let response = self
            .command(&list_command(self.advertised.special_use, true))
            .await?;
        // Untagged only: `LIST` and `STATUS` data never rides the completion line, and
        // reading it as though it might invents a mailbox out of the server's prose.
        let lines = response.untagged();
        let rows = crate::parse::parse_list(lines)?;
        Ok((rows, parse_status_unseen(lines)))
    }

    /// `STATUS <mailbox> (UNSEEN)` — one mailbox's unread count, or `None` if the
    /// server answered without one.
    ///
    /// # Errors
    ///
    /// The classified failure of the `STATUS` command. A `NO` is *not* special-cased
    /// here; [`unseen_by_probing`] decides what a refused mailbox means.
    pub(crate) async fn status_unseen(&mut self, mailbox: &str) -> ImapResult<Option<u32>> {
        let response = self
            .command(&format!("STATUS {} (UNSEEN)", quote(mailbox)))
            .await?;
        Ok(parse_status_unseen(response.untagged())
            .get(mailbox)
            .copied())
    }
}

/// Reads every `* STATUS <mailbox> (… UNSEEN <n> …)` line into a mailbox → count map.
///
/// Skips lines that are not a `STATUS`, and `STATUS` lines whose attribute list carries
/// no `UNSEEN` — a server may return a subset, and a missing count must stay missing
/// rather than becoming a zero. The attribute list is read as pairs, so `UNSEEN` is
/// found wherever it sits among `MESSAGES`/`UIDNEXT`/`UIDVALIDITY`.
pub(crate) fn parse_status_unseen(lines: &[Vec<u8>]) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for line in lines {
        let Ok(items) = items_of(line) else {
            continue;
        };
        // `STATUS <mailbox> (<attr> <value> …)`
        let [keyword, mailbox, attributes, ..] = items.as_slice() else {
            continue;
        };
        if !keyword
            .as_atom()
            .is_some_and(|atom| atom.eq_ignore_ascii_case("STATUS"))
        {
            continue;
        }
        let (Some(name), Some(attributes)) = (mailbox.as_nstring(), attributes.as_list()) else {
            continue;
        };
        if let Some(count) = unseen_of(attributes) {
            counts.insert(name, count);
        }
    }
    counts
}

/// The `UNSEEN` value from a `STATUS` attribute list, or `None` when the server did
/// not return that attribute.
fn unseen_of(attributes: &[Item]) -> Option<u32> {
    attributes
        .chunks_exact(2)
        .find(|pair| {
            pair[0]
                .as_atom()
                .is_some_and(|atom| atom.eq_ignore_ascii_case("UNSEEN"))
        })
        .and_then(|pair| pair[1].as_atom())
        .and_then(|value| value.parse().ok())
}

/// Fills in every listed mailbox's unread count with one `STATUS` each — the fallback
/// for a server without `LIST-STATUS`.
///
/// `\Noselect` rows are skipped (they are hierarchy nodes, and `STATUS` on one is an
/// error, not a zero), and a mailbox the server *refuses* (`NO`/`BAD`) is left uncounted
/// rather than failing the whole folder list — one unreadable folder must not cost the
/// user every other folder's badge. Probing stops at [`MAX_STATUS_PROBES`].
///
/// A transport failure does propagate: every further probe would be issued down a dead
/// connection, and returning what was collected so far would report a half-counted
/// folder list as a complete one.
///
/// # Errors
///
/// [`ImapError::Io`](crate::error::ImapError::Io) or another non-refusal failure of a
/// `STATUS` command.
pub(crate) async fn unseen_by_probing<S>(
    connection: &mut Connection<S>,
    rows: &[ListRow],
) -> ImapResult<HashMap<String, u32>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut counts = HashMap::new();
    for row in rows
        .iter()
        .filter(|row| !has_noselect(&row.attributes))
        .take(MAX_STATUS_PROBES)
    {
        match connection.status_unseen(&row.name).await {
            Ok(Some(count)) => {
                counts.insert(row.name.clone(), count);
            }
            Ok(None) | Err(ImapError::No(_) | ImapError::Bad(_)) => {}
            Err(other) => return Err(other),
        }
    }
    Ok(counts)
}

/// Whether a `LIST` row carries `\Noselect` — a container that holds no messages.
fn has_noselect(attributes: &[String]) -> bool {
    attributes
        .iter()
        .any(|attribute| attribute.eq_ignore_ascii_case("\\Noselect"))
}

#[cfg(test)]
#[path = "unseen_tests.rs"]
mod tests;
