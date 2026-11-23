//! A widening's band over one folder: the folder's older mail a wider window adds, fetched
//! without giving up the delta cursor (`docs/design/windows-and-widening.md`, §4.2).
//!
//! The band is [`Widening::band_for`] the bound folder, `[since, before)`. It is found with one
//! search, `UID SEARCH UID 1:<UIDNEXT − 1> SINCE <since> BEFORE <before>` (`SINCE` left out for a
//! band reaching the whole history):
//!
//! - **The band reaches one server day past each of its ends**: `SINCE` names the day before the
//!   new floor and `BEFORE` the day after the old one. `SINCE` and `BEFORE` compare the server's
//!   own day of a message's `INTERNALDATE` (RFC 9051 §6.4.4 disregards its time and zone; Dovecot
//!   reads the day in the server's zone), while the engine judges a message by its UTC day
//!   ([`MailboxWindows::admits`](engine_core::sync::MailboxWindows::admits)). UTC offsets run from
//!   −12 to +14 hours, so the two days differ by at most one, either way: a server ahead of UTC
//!   dates a message near a floor a day later than the engine, one behind it a day earlier.
//!   Measured on Dovecot in UTC+14: a band ending exactly `BEFORE` the old floor left out a message
//!   the first pass had fetched and the engine had dropped. What the extra days list beyond the
//!   band is dropped by the engine's filter (additive chunks) or by `crate::stream` (a fresh pass's
//!   reconcile), or is held mail listed again, the same keys.
//! - **The UID bound is the kept cursor's `UIDNEXT`**, so the band and the next delta are disjoint:
//!   a message arriving during the band has a higher UID and comes through the delta, judged by the
//!   wider window.
//! - A server that cuts a search off at its `MESSAGELIMIT` is answered from each message's own
//!   `INTERNALDATE`, with both bounds ([`Connection::uid_search_dated`]).
//!
//! The UIDs are fetched in the cold backfill's descending groups ([`backfill_scan`]), as
//! [`PassMode::Additive`](engine_provider::PassMode::Additive) chunks that tombstone nothing.
//! Each group's boundary checkpoints the **band watermark**, the lowest UID committed, beside the
//! kept cursor (`;w<low>`); the last chunk advances to the kept cursor alone.
//!
//! **The band continues below a watermark only when the caller says the widening resumes**
//! ([`Widening::resumes`]): only the caller knows whether a pass under a narrower window ran in
//! between and dropped mail the band had not reached. Otherwise the watermark is ignored and the
//! band starts afresh. The watermark rides through every delta on the same UID space ([`carried`]),
//! a reconciling one included: asked under the depth the band widens to, as the orchestrator asks
//! it, the reconcile lists every message the band stored and tombstones none of them, so a band on
//! a server without QRESYNC, which reconciles on every pass, still continues. A `UIDVALIDITY` that
//! moved voids the cursor: the folder is fetched afresh over the wider window.

use std::sync::Arc;

use engine_core::{
    ids::MailboxId,
    sync::{SyncState, Widening},
};
use engine_provider::{EmailChunk, EmailStream, ProviderResult};
use futures_util::{Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    cursor::MailboxCursor,
    pool::ImapPool,
    provider::ImapProvider,
    stream::{Completion, backfill_scan, imap_date, list_groups},
    transport::Connection,
    transport_search::DatedSearch,
};

/// `next`, the cursor a delta ends on, with `prior`'s band watermark carried when both name one
/// UID space, so the band it records can still be continued.
pub(crate) fn carried(next: SyncState, prior: Option<MailboxCursor>) -> SyncState {
    let Some(prior) = prior.filter(|prior| prior.band_low.is_some()) else {
        return next;
    };
    match MailboxCursor::decode(&next) {
        Some(cursor) if cursor.uid_validity == prior.uid_validity => MailboxCursor {
            band_low: prior.band_low,
            ..cursor
        }
        .encode(),
        _ => next,
    }
}

/// Streams the bound folder's band of `widening`, keeping `cursor`. See the module docs.
#[allow(clippy::too_many_arguments)] // the band's inputs are all distinct
pub(crate) fn widen_email<'a, S>(
    conn: &'a mut Connection<S>,
    pool: Option<&'a Arc<ImapPool<S>>>,
    mailbox: &'a MailboxId,
    cursor: &'a SyncState,
    widening: &'a Widening,
    fetch_batch: usize,
    chunk_size: usize,
) -> impl Stream<Item = ProviderResult<EmailChunk>> + Send + 'a
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    async_stream::try_stream! {
        let fetch_batch = conn.negotiated.within_message_limit(fetch_batch);
        let select = if conn.qresync_enabled() {
            conn.select_condstore(mailbox.as_str()).await?
        } else {
            conn.select(mailbox.as_str()).await?
        };
        let prior = MailboxCursor::current(cursor)
            .filter(|prior| prior.uid_validity == select.uid_validity);
        let Some(prior) = prior else {
            // The UID space moved, or the cursor is not one this sync wrote: it holds nothing a
            // band could continue from, so the folder is fetched afresh under the wider window.
            let window = widening.to().window_for(mailbox);
            let fresh = crate::stream::stream_email(
                conn, pool, mailbox, None, window, fetch_batch, chunk_size,
            );
            for await chunk in fresh {
                yield chunk?;
            }
            return;
        };
        let kept = MailboxCursor {
            band_low: None,
            ..prior
        };
        let band = widening.band_for(mailbox);
        let Some((since, before)) = band.and_then(|band| Some((band.since(), band.before()?)))
        else {
            // This folder's window gains nothing.
            yield EmailChunk::additive(Vec::new(), Vec::new(), Some(0), kept.encode());
            return;
        };
        // The caller decides whether this band continues one it recorded.
        let resume_below = prior.band_low.filter(|_| widening.resumes());
        let highest = resume_below.unwrap_or(prior.uid_next).saturating_sub(1);
        // A server day past each end: see the module docs.
        let since_text = since
            .map(|since| imap_date(since.checked_add_days(-1).unwrap_or(since)))
            .transpose()?;
        let before_text = imap_date(before.checked_add_days(1).unwrap_or(before))?;
        let mut uids = if highest == 0 {
            Vec::new()
        } else {
            conn.uid_search_dated(DatedSearch {
                since: since_text.as_deref(),
                before: Some(&before_text),
                highest: Some(highest),
            })
            .await?
        };
        uids.retain(|&uid| uid <= highest);
        uids.sort_unstable_by(|a, b| b.cmp(a));
        let total = Some(uids.len());
        if uids.is_empty() {
            yield EmailChunk::additive(Vec::new(), Vec::new(), total, kept.encode());
            return;
        }
        let groups = list_groups(&uids, fetch_batch);
        let completion = Completion::Band { kept };
        let scan = backfill_scan(
            conn,
            pool,
            mailbox,
            select.uid_validity,
            groups,
            completion,
            total,
            chunk_size,
        );
        for await chunk in scan {
            yield chunk?;
        }
    }
}

impl<S> ImapProvider<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    /// [`widen_email`] over a pooled session on the bound folder, for
    /// [`Provider::widen_email`](engine_provider::Provider::widen_email).
    pub(crate) fn band_stream<'a>(
        &'a self,
        cursor: &'a SyncState,
        widening: &'a Widening,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        Box::pin(async_stream::stream! {
            let mut connection = match self.folder_session().await {
                Ok(connection) => connection,
                Err(err) => {
                    yield Err(err);
                    return;
                }
            };
            // Handed back healthy only if every item was, as `stream_email`'s.
            let mut failed = false;
            {
                let band = widen_email(
                    &mut connection,
                    Some(&self.pool),
                    &self.mailbox,
                    cursor,
                    widening,
                    fetch_batch,
                    chunk_size,
                );
                futures_util::pin_mut!(band);
                while let Some(item) = band.next().await {
                    failed = item.is_err();
                    yield item;
                }
            }
            if failed {
                connection.discard();
            }
        })
    }
}

#[cfg(test)]
#[path = "band_tests.rs"]
mod tests;

#[cfg(all(test, feature = "import"))]
#[path = "band_live_tests.rs"]
mod live_tests;
