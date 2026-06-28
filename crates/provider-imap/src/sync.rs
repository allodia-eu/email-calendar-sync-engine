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

use std::cmp::Reverse;

use engine_core::ids::MailboxId;
use engine_core::mail::Message;
use engine_core::sync::SyncState;
use engine_provider::{PageToken, SyncKind, SyncPage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::cursor::{self, MailboxCursor};
use crate::error::ImapResult;
use crate::mail::message_from_fetch;
use crate::parse::{FetchRow, SelectData};
use crate::transport::Connection;

/// The metadata `FETCH` items — Tier-1, all peek-safe (none sets `\Seen`).
///
/// `BODY.PEEK[HEADER.FIELDS (REFERENCES)]` carries the `References` header, which
/// `ENVELOPE` omits (RFC 9051 §7.5.2) — it is what local threading needs. The peek
/// form is required so the read does not set `\Seen`; the server echoes it back as
/// `BODY[HEADER.FIELDS (REFERENCES)]`.
const FETCH_ITEMS: &str =
    "UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODY.PEEK[HEADER.FIELDS (REFERENCES)]";

/// Fetches one page of the bound mailbox's mail since `cursor`, continuing from
/// `page` (a UID boundary) and bounded by `limit` (`0` means the whole window in
/// one page).
///
/// `since` is the optional sync-depth window floor (an IMAP `dd-Mon-yyyy` date): when
/// set, a **snapshot** fetches only mail delivered on or after it (found via
/// `UID SEARCH SINCE`), so a large mailbox syncs just recent messages. It never
/// narrows a delta — new arrivals are recent by definition — nor changes paging once
/// the floor is set.
pub(crate) async fn sync_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    limit: usize,
    since: Option<&str>,
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

    // A sync-depth window bounds the snapshot to recent mail: fetch **only** the UIDs that
    // `UID SEARCH SINCE` reports in window — never the whole UID range above the oldest of
    // them. That range can span most of the mailbox when moved/imported mail scrambles the
    // UID-vs-date order (the cause of a 3-month window still pulling tens of thousands of
    // old messages, and of `fetched` overshooting the in-window `total`). A delta is
    // already new-arrivals-only, so the window never applies to it.
    if let (SyncKind::Snapshot, Some(date)) = (kind, since) {
        let mut in_window = conn.uid_search_since(date).await?;
        if in_window.is_empty() {
            return Ok(empty_page(SyncKind::Snapshot, next_cursor, Some(0)));
        }
        in_window.sort_unstable();
        return windowed_snapshot_page(
            conn,
            mailbox,
            uid_validity,
            next_cursor,
            &in_window,
            page,
            limit,
        )
        .await;
    }

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

    // Collect up to `limit` messages, newest UID first, widening the UID window
    // downward over gaps (expunged UIDs) so a page carries a full `limit` of
    // *messages* — not a `limit`-wide span of UID slots that gaps could leave
    // near-empty. `limit == 0` means "the whole remaining window in one page" (the
    // drain default).
    let target = if limit == 0 { usize::MAX } else { limit };
    let chunk = if limit == 0 {
        high - low_bound + 1
    } else {
        u32::try_from(limit).unwrap_or(u32::MAX)
    };

    let mut rows: Vec<FetchRow> = Vec::new();
    let mut window_high = high;
    let reached_floor = loop {
        let window_low = low_bound.max(window_high.saturating_sub(chunk.saturating_sub(1)));
        let mut fetched = conn
            .uid_fetch(&format!("{window_low}:{window_high}"), FETCH_ITEMS)
            .await?;
        rows.append(&mut fetched);
        if window_low == low_bound {
            break true;
        }
        if rows.len() >= target {
            break false;
        }
        window_high = window_low - 1;
    };

    // Keep the newest `target` messages; any older overshoot is re-fetched by the
    // next page (whose window ends strictly below the lowest kept UID, so no
    // duplication).
    rows.sort_unstable_by_key(|row| row.uid);
    let overshoot = rows.len() > target;
    let start = rows.len().saturating_sub(target);
    let kept = &rows[start..];

    // `FETCH` returns ascending UID; reverse so the page renders newest-first.
    let messages: Vec<Message> = kept
        .iter()
        .rev()
        .map(|row| message_from_fetch(row, mailbox, uid_validity))
        .collect();
    let present = match kind {
        SyncKind::Snapshot => messages.iter().map(|m| m.id.key().clone()).collect(),
        SyncKind::Delta => Vec::new(),
    };

    // There is more below this page iff we capped an overshoot or stopped before
    // reaching the floor; the next window ends just below the lowest kept UID.
    let more_below = overshoot || !reached_floor;
    let next_page = if more_below {
        kept.first()
            .and_then(|row| row.uid.checked_sub(1))
            .filter(|&boundary| boundary >= low_bound)
            .map(cursor::page_token)
    } else {
        None
    };

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

/// Fetches one page of a **sync-depth-windowed** snapshot: only the UIDs `UID SEARCH
/// SINCE` reported in window (`in_window`, ascending), newest-first, `limit` per page
/// (`0` = the whole remaining window). So a large mailbox downloads exactly the recent
/// mail — never the whole UID range above the oldest in-window message — and `fetched`
/// can never overshoot the in-window `total`. `page` is the exclusive high boundary (the
/// lowest UID the prior page kept); `None` starts from the newest.
async fn windowed_snapshot_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    uid_validity: u32,
    next_cursor: SyncState,
    in_window: &[u32],
    page: Option<&PageToken>,
    limit: usize,
) -> ImapResult<SyncPage<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let total = Some(in_window.len());
    // Resume below the prior page's lowest kept UID (the opaque boundary), newest-first.
    let boundary = page.and_then(cursor::page_high);
    let mut pending: Vec<u32> = in_window
        .iter()
        .copied()
        .filter(|&uid| boundary.is_none_or(|b| uid < b))
        .collect();
    pending.sort_unstable_by_key(|&uid| Reverse(uid)); // descending: newest first
    let take = if limit == 0 {
        pending.len()
    } else {
        limit.min(pending.len())
    };
    let chunk = &pending[..take];
    if chunk.is_empty() {
        // Drained: a final empty page; the streaming loop's accumulated `present` (every
        // in-window key) is what tombstones anything now outside the window.
        return Ok(empty_page(SyncKind::Snapshot, next_cursor, total));
    }
    // Fetch exactly these UIDs as a compact set (`5,7,10:12`), so the request — and the
    // download — is bounded to the window, not a range spanning the whole mailbox.
    let mut set = chunk.to_vec();
    set.sort_unstable();
    let mut rows = conn.uid_fetch(&uid_set_spec(&set), FETCH_ITEMS).await?;
    rows.sort_unstable_by_key(|row| Reverse(row.uid)); // newest first for display
    let messages: Vec<Message> = rows
        .iter()
        .map(|row| message_from_fetch(row, mailbox, uid_validity))
        .collect();
    let present = messages.iter().map(|m| m.id.key().clone()).collect();
    // More remains iff any in-window UID falls below the lowest we just took.
    let lowest = *chunk.iter().min().expect("chunk is non-empty");
    let next_page = in_window
        .iter()
        .any(|&uid| uid < lowest)
        .then(|| cursor::page_token(lowest));
    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        changed: messages,
        removed: Vec::new(),
        present,
        next_page,
        next_cursor,
        total,
    })
}

/// Compacts a **sorted-ascending** UID list into an IMAP sequence-set (`5,7,10:12`),
/// collapsing contiguous runs into ranges so the `UID FETCH` command stays short even
/// for a few hundred UIDs.
fn uid_set_spec(sorted: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            end = sorted[index + 1];
            index += 1;
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        index += 1;
    }
    parts.join(",")
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
