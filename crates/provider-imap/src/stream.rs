//! Streaming email sync — the incremental, resumable counterpart to [`crate::sync`].
//!
//! [`stream_email`] `SELECT`s the bound mailbox once and splits into two paths:
//!
//! - A **cold backfill** (a first sync, or one resuming below a prior watermark) is streamed here:
//!   it descends newest-UID-first, pulls each `UID FETCH` group **one row at a time**
//!   ([`Connection::next_fetch_row`]) so a chunk commits every `chunk_size` messages *within* one
//!   batched fetch (visible before the whole group downloads). The cursor's `backfill_low`
//!   watermark advances at each **fetch-batch group boundary**, so a kill resumes below the last
//!   completed group (re-fetching at most that group's rows, which re-upsert idempotently) — commit
//!   granularity is `chunk_size`; resume granularity is `fetch_batch` (`store-and-sync.md`). This
//!   is the responsive, resumable path a large mailbox needs. A pass that **starts fresh** (no
//!   prior watermark) sees the whole in-window set in one run, so its completing chunk
//!   **reconciles** against the accumulated present set — tombstoning local rows the server no
//!   longer has (the `reset`/`clear`/normalizer contract); a *resumed* pass completes additively
//!   (its present set is partial), deferring the reconcile to the next uninterrupted pass.
//! - A **delta** (new arrivals, or a QRESYNC flag/expunge reconcile) or a **`UIDVALIDITY`-reset
//!   re-snapshot** is delegated to the battle-tested [`crate::sync::sync_page`] and re-chunked with
//!   [`split_page`]. These are small (a delta) or rare (a reset), so fetching a page whole before
//!   re-chunking is fine.
//!
//! Previews are **not** hydrated here (reading bodies would defeat fast metadata
//! streaming); a host fetches bodies on demand for the rows it shows.
//!
//! The returned stream holds the connection's [`Mutex`](tokio::sync::Mutex) guard for
//! its whole lifetime (the session is stateful and sequential — one command at a
//! time), so a host must drive **one** email stream over a given connection at a time;
//! it must not poll two concurrently over the same connection (each folder gets its
//! own connection, so the per-folder fan-out is unaffected).

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::Message,
    sync::{SyncState, SyncWindow},
};
use engine_provider::{EmailChunk, PassMode, ProviderError, ProviderResult, SyncKind, split_page};
use futures_util::Stream;
use time::Month;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
};

use crate::{
    cursor::MailboxCursor,
    mail::message_from_fetch,
    sync::{FETCH_ITEMS, effective_uid_next, sync_page_selected, uid_set_spec},
    transport::Connection,
    transport_command::format_imap_date,
};

/// Streams the bound mailbox's email for one pass. See the module docs.
pub(crate) fn stream_email<'a, S>(
    connection: &'a Mutex<Connection<S>>,
    mailbox: &'a MailboxId,
    cursor: Option<&'a SyncState>,
    window: SyncWindow,
    fetch_batch: usize,
    chunk_size: usize,
) -> impl Stream<Item = ProviderResult<EmailChunk>> + Send + 'a
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async_stream::try_stream! {
        let mut conn = connection.lock().await;
        let qresync = conn.qresync_enabled();
        let select = if qresync {
            conn.select_condstore(mailbox.as_str()).await?
        } else {
            conn.select(mailbox.as_str()).await?
        };
        let uid_validity = select.uid_validity;
        let uid_next = effective_uid_next(&mut conn, &select).await?;
        let prior = cursor.and_then(MailboxCursor::decode);
        let since = imap_since(window)?;

        // A cold backfill: a first sync (no cursor), or one resuming below a prior
        // watermark under the same UID space. Anything else — a delta, or a reset
        // (UIDVALIDITY changed) — goes through the tested page path below.
        let backfill = match prior {
            None => Some(uid_next.saturating_sub(1)),
            Some(p) if p.uid_validity == uid_validity => {
                p.backfill_low.map(|low| low.saturating_sub(1))
            }
            Some(_) => None,
        };

        if let Some(high) = backfill {
            // The frontier the checkpoint records: the UIDNEXT **and** HIGHESTMODSEQ
            // captured when the backfill first started (so mail that arrives, and flag/
            // expunge changes that happen, *during* the backfill are caught by the first
            // delta afterwards), preserved across resumes — the resumed SELECT's fresher
            // values would otherwise skip everything that changed since the kill.
            let frontier = MailboxCursor {
                uid_validity,
                uid_next: prior.map_or(uid_next, |p| p.uid_next),
                highest_modseq: prior.and_then(|p| p.highest_modseq).or(select.highest_modseq),
                backfill_low: None,
            };
            // A backfill that STARTS fresh this session (no prior watermark) sees the
            // whole in-window set in one run, so its completing chunk reconciles: it
            // tombstones local rows the server no longer has (the `reset`/`clear`/
            // normalizer contract). A *resumed* backfill only saw part of the set this
            // session, so it completes additively (no tombstone) — the reconcile lands
            // on the next uninterrupted pass.
            let is_fresh = prior.is_none();
            let (groups, windowed_total) =
                backfill_groups(&mut conn, high, since.as_deref(), fetch_batch).await?;
            // The progress denominator: the in-window count when bounded, else the
            // mailbox's message count from SELECT.
            let total = windowed_total
                .or_else(|| Some(usize::try_from(select.exists).unwrap_or(usize::MAX)));
            let empty = groups.is_empty();
            let scan = backfill_scan(
                &mut conn,
                mailbox,
                uid_validity,
                groups,
                &frontier,
                total,
                chunk_size,
                is_fresh,
            );
            for await chunk in scan {
                yield chunk?;
            }
            // An empty mailbox has no group to carry the completing cursor, so mark it
            // here (the last group already advances to `frontier` when non-empty). A
            // fresh pass reconciles against the empty present set (tombstoning every
            // stale local row); a resume just completes.
            if empty {
                yield if is_fresh {
                    EmailChunk::reconcile_last(Vec::new(), Vec::new(), total, frontier.encode())
                } else {
                    EmailChunk::additive(Vec::new(), Vec::new(), total, frontier.encode())
                };
            }
            return;
        }

        // Delta or reset: drain the page primitive and re-chunk. The pass mode is
        // decided once — a snapshot (reset) reconciles/tombstones, a delta is additive.
        let mut page_token = None;
        let mut mode: Option<PassMode> = None;
        let mut total: Option<usize> = None;
        let final_cursor = loop {
            // Reuse the SELECT already done above — the mailbox stays selected across
            // the pass, so the page path must not re-SELECT.
            let page = sync_page_selected(
                &mut conn,
                mailbox,
                &select,
                uid_next,
                cursor,
                page_token.as_ref(),
                fetch_batch,
                since.as_deref(),
            )
            .await?;
            total = total.or(page.total);
            let pass_mode = *mode.get_or_insert(match page.kind {
                SyncKind::Snapshot => PassMode::Reconcile,
                SyncKind::Delta => PassMode::Additive,
            });
            let is_last = page.next_page.is_none();
            let next_cursor = page.next_cursor.clone();
            let chunks = split_page(
                pass_mode,
                page.changed,
                page.patched,
                page.removed,
                page.present,
                total,
                chunk_size,
            );
            for chunk in chunks {
                yield chunk;
            }
            if is_last {
                break next_cursor;
            }
            page_token = page.next_page;
        };
        let marker = match mode.unwrap_or(PassMode::Additive) {
            PassMode::Additive => {
                EmailChunk::additive(Vec::new(), Vec::new(), total, final_cursor)
            }
            PassMode::Reconcile => {
                EmailChunk::reconcile_last(Vec::new(), Vec::new(), total, final_cursor)
            }
        };
        yield marker;
    }
}

/// Streams the backfill's descending UID `groups` row by row, emitting an additive
/// chunk every `chunk_size` messages and checkpointing `backfill_low` = each group's
/// lowest UID at its boundary, so a crash resumes below it. `frontier` carries the
/// `UIDNEXT`/modseq the checkpoint records.
///
/// When `is_fresh` (the backfill started with no prior watermark), it accumulates the
/// server's full in-window key set as it goes and makes the **last** group's chunk a
/// [`reconcile_last`](EmailChunk::reconcile_last) carrying it, so the store tombstones
/// local rows the server no longer has. A resumed backfill (`is_fresh` false) saw only
/// part of the set this session, so it completes additively (no tombstone).
#[allow(clippy::too_many_arguments)] // the fetch inputs plus the reconcile flag are all distinct
fn backfill_scan<'c, S>(
    conn: &'c mut Connection<S>,
    mailbox: &'c MailboxId,
    uid_validity: u32,
    groups: Vec<(String, u32)>,
    frontier: &'c MailboxCursor,
    total: Option<usize>,
    chunk_size: usize,
    is_fresh: bool,
) -> impl Stream<Item = ProviderResult<EmailChunk>> + 'c
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    async_stream::try_stream! {
        let last_index = groups.len().saturating_sub(1);
        // The full in-window key set, accumulated across groups for the fresh-pass
        // reconcile (left empty on a resume, which does not tombstone).
        let mut present: Vec<ProviderKey> = Vec::new();
        for (index, (spec, group_low)) in groups.into_iter().enumerate() {
            conn.uid_fetch_stream_start(&spec, FETCH_ITEMS).await?;
            let mut buf: Vec<Message> = Vec::new();
            while let Some(row) = conn.next_fetch_row().await? {
                let message = message_from_fetch(&row, mailbox, uid_validity);
                if is_fresh {
                    present.push(message.id.key().clone());
                }
                buf.push(message);
                if chunk_size != 0 && buf.len() >= chunk_size {
                    // An intermediate chunk of this group: visible, cursor held.
                    yield EmailChunk::additive_held(core::mem::take(&mut buf), Vec::new(), total);
                }
            }
            let changed = core::mem::take(&mut buf);
            if index == last_index {
                // The completing chunk advances to `frontier` (steady state) and, for a
                // fresh pass, reconciles against the accumulated present set.
                yield if is_fresh {
                    EmailChunk::reconcile_last(
                        changed,
                        core::mem::take(&mut present),
                        total,
                        frontier.encode(),
                    )
                } else {
                    EmailChunk::additive(changed, Vec::new(), total, frontier.encode())
                };
            } else {
                // An intermediate group checkpoints its lowest UID (the resume point).
                let checkpoint = MailboxCursor { backfill_low: Some(group_low), ..*frontier };
                yield EmailChunk::additive(changed, Vec::new(), total, checkpoint.encode());
            }
        }
    }
}

/// The descending UID groups a backfill fetches: the sync-depth window's UIDs (via
/// `UID SEARCH SINCE`) when bounded, else the full `1..=high` range chunked by
/// `fetch_batch`. Each entry is `(compact set spec, lowest UID)`, newest group first.
async fn backfill_groups<S>(
    conn: &mut Connection<S>,
    high: u32,
    since: Option<&str>,
    fetch_batch: usize,
) -> ProviderResult<(Vec<(String, u32)>, Option<usize>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if high == 0 {
        return Ok((Vec::new(), since.map(|_| 0)));
    }
    match since {
        Some(date) => {
            let mut uids: Vec<u32> = conn
                .uid_search_since(date)
                .await?
                .into_iter()
                .filter(|&uid| uid <= high)
                .collect();
            uids.sort_unstable_by(|a, b| b.cmp(a)); // descending
            let total = uids.len();
            Ok((list_groups(&uids, fetch_batch), Some(total)))
        }
        // A full range's exact count (over UID gaps) is unknown here; the caller
        // supplies the mailbox's message count from SELECT as the denominator.
        None => Ok((full_range_groups(high, 1, fetch_batch), None)),
    }
}

/// Descending contiguous UID ranges of `fetch_batch` width covering `[low_floor, high]`.
fn full_range_groups(high: u32, low_floor: u32, fetch_batch: usize) -> Vec<(String, u32)> {
    if high < low_floor {
        return Vec::new();
    }
    let step = if fetch_batch == 0 {
        high - low_floor + 1
    } else {
        u32::try_from(fetch_batch).unwrap_or(u32::MAX)
    };
    let mut groups = Vec::new();
    let mut window_high = high;
    loop {
        let window_low = low_floor.max(window_high.saturating_sub(step.saturating_sub(1)));
        groups.push((format!("{window_low}:{window_high}"), window_low));
        if window_low == low_floor {
            break;
        }
        window_high = window_low - 1;
    }
    groups
}

/// Groups a descending UID list into compact set specs of `fetch_batch` UIDs each.
fn list_groups(uids_desc: &[u32], fetch_batch: usize) -> Vec<(String, u32)> {
    let step = if fetch_batch == 0 {
        uids_desc.len().max(1)
    } else {
        fetch_batch
    };
    uids_desc
        .chunks(step)
        .map(|chunk| {
            let mut sorted = chunk.to_vec();
            sorted.sort_unstable();
            let low = *sorted.first().expect("chunks() yields no empty slice");
            (uid_set_spec(&sorted), low)
        })
        .collect()
}

/// The IMAP `dd-Mon-yyyy` floor for a sync-depth window, or `None` for the full
/// history.
fn imap_since(window: SyncWindow) -> ProviderResult<Option<String>> {
    let Some(date) = window.floor() else {
        return Ok(None);
    };
    let month = Month::try_from(date.month())
        .map_err(|_| ProviderError::invalid_state("sync window has an invalid month"))?;
    let date = time::Date::from_calendar_date(date.year(), month, date.day())
        .map_err(|_| ProviderError::invalid_state("sync window is not a real date"))?;
    Ok(Some(format_imap_date(date)))
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
