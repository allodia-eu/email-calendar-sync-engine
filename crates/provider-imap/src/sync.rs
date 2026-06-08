//! IMAP snapshot/delta paging — the per-mailbox sync logic behind
//! [`Provider::sync_email_page`](engine_provider::Provider::sync_email_page).
//!
//! Each call `SELECT`s the bound mailbox (reading its current `UIDVALIDITY`/
//! `UIDNEXT`) and fetches one **UID window**, newest UIDs first, so a streaming host
//! renders recent mail first. The window descends across pages; the next boundary
//! travels in the opaque [`PageToken`]. The pass is:
//!
//! - a **snapshot** on the first sync (no cursor) or when `UIDVALIDITY` changed
//!   since the cursor (a reset — the IMAP analogue of JMAP `cannotCalculateChanges`):
//!   every existing UID is rediscovered and carried in `present`, so the store
//!   tombstones whatever is now absent (expunged or renumbered);
//! - a **delta** otherwise: only UIDs at or above the cursor's `UIDNEXT` (new
//!   arrivals). Flag changes and expunges of already-synced messages are **not**
//!   reported in a delta — detecting them incrementally needs CONDSTORE/QRESYNC (a
//!   deferred capability); a periodic snapshot reconciles them. So a delta never
//!   carries removals.

use engine_core::ids::MailboxId;
use engine_core::mail::Message;
use engine_core::sync::SyncState;
use engine_provider::{PageToken, SyncKind, SyncPage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cursor::{self, MailboxCursor};
use crate::error::ImapResult;
use crate::mail::message_from_fetch;
use crate::parse::SelectData;
use crate::transport::Connection;

/// The metadata `FETCH` items — Tier-1, all peek-safe (none sets `\Seen`).
const FETCH_ITEMS: &str = "UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE";

/// Fetches one page of the bound mailbox's mail since `cursor`, continuing from
/// `page` (a UID boundary) and bounded by `limit` (`0` means the whole window in
/// one page).
pub(crate) async fn sync_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    limit: usize,
) -> ImapResult<SyncPage<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let select = conn.select(mailbox.as_str()).await?;
    let uid_validity = select.uid_validity;
    let uid_next = effective_uid_next(conn, &select).await?;
    let next_cursor = MailboxCursor {
        uid_validity,
        uid_next,
    }
    .encode();

    // First sync, or a UIDVALIDITY reset, is a snapshot from UID 1; a matching
    // cursor is a delta from its watermark.
    let (kind, low_bound) = match cursor.and_then(MailboxCursor::decode) {
        Some(prior) if prior.uid_validity == uid_validity => (SyncKind::Delta, prior.uid_next),
        _ => (SyncKind::Snapshot, 1),
    };
    let total = match kind {
        SyncKind::Snapshot => Some(usize::try_from(select.exists).unwrap_or(usize::MAX)),
        SyncKind::Delta => None,
    };

    // The highest UID this page covers: the continuation boundary, else the newest.
    let newest = uid_next.saturating_sub(1);
    let high = page
        .and_then(cursor::page_high)
        .unwrap_or(newest)
        .min(newest);
    if high < low_bound {
        // An empty mailbox snapshot, or a delta with no new arrivals.
        return Ok(empty_page(kind, next_cursor, total));
    }

    let width = if limit == 0 {
        high - low_bound + 1
    } else {
        u32::try_from(limit).unwrap_or(u32::MAX)
    };
    let page_low = low_bound.max(high.saturating_sub(width.saturating_sub(1)));

    let rows = conn
        .uid_fetch(&format!("{page_low}:{high}"), FETCH_ITEMS)
        .await?;
    // `FETCH` returns ascending UID; reverse so the page renders newest-first.
    let messages: Vec<Message> = rows
        .iter()
        .rev()
        .map(|row| message_from_fetch(row, mailbox, uid_validity))
        .collect();
    let present = match kind {
        SyncKind::Snapshot => messages.iter().map(|m| m.id.key().clone()).collect(),
        SyncKind::Delta => Vec::new(),
    };
    // Continue while the next window still has UIDs at or above the low bound.
    let next_page = page_low
        .checked_sub(1)
        .filter(|&boundary| boundary >= low_bound)
        .map(cursor::page_token);

    Ok(SyncPage {
        kind,
        changed: messages,
        removed: Vec::new(),
        present,
        next_page,
        next_cursor,
        total,
    })
}

/// A pass with no messages in range (empty mailbox snapshot, or a no-arrivals
/// delta): a single page that advances the cursor and tombstones via the empty
/// `present` (snapshot) or carries nothing (delta).
fn empty_page(kind: SyncKind, next_cursor: SyncState, total: Option<usize>) -> SyncPage<Message> {
    SyncPage {
        kind,
        changed: Vec::new(),
        removed: Vec::new(),
        present: Vec::new(),
        next_page: None,
        next_cursor,
        total,
    }
}

/// The mailbox's `UIDNEXT`: the advertised value, or — when the server omits it —
/// one past the highest existing UID (`1` for an empty mailbox).
async fn effective_uid_next<S>(conn: &mut Connection<S>, select: &SelectData) -> ImapResult<u32>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Some(uid_next) = select.uid_next {
        return Ok(uid_next);
    }
    if select.exists == 0 {
        return Ok(1);
    }
    let rows = conn.uid_fetch("*", "UID").await?;
    Ok(rows
        .iter()
        .map(|row| row.uid)
        .max()
        .map_or(1, |highest| highest.saturating_add(1)))
}

#[cfg(test)]
#[path = "sync_tests.rs"]
mod tests;
