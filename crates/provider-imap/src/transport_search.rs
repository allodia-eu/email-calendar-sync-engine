//! Finding a sync-depth window's messages when a server will not say how many there are.
//!
//! A server advertising RFC 9738 `MESSAGELIMIT` may answer a search matching more messages
//! than its limit with only some of them. Yahoo answers `UID SEARCH SINCE` with the newest
//! `MESSAGELIMIT` matches by date and a plain `OK`, in either mode, and applies that cut before
//! any other criterion, so neither narrowing by UID nor its `PARTIAL` (`BAD` for any range not
//! starting at 1) reaches the rest. A search that returns as many matches as the limit, or
//! that is refused with `NO [LIMIT]`, is therefore read as cut off, and the window is decided from
//! each message's own `INTERNALDATE`, fetched no more than the limit at a time. A window under the
//! limit, or a server stating none, keeps the single search.
//!
//! The dates are paged with `PARTIAL` (RFC 9394) where the server offers it: each
//! `UID FETCH <low>:* … (PARTIAL 1:<limit>)` returns the next `limit` messages, so the pages
//! number the messages, not the UIDs. Without it they are UID spans of the limit's width, which
//! on a mailbox whose UIDs run far past its message count costs a command per span.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    capability::Extension,
    error::{ImapError, ImapResult},
    parse::{self, FetchRow},
    transport::{Connection, opens_with_code},
    transport_command::imap_day,
};

/// What the fallback reads of each message: its date, which decides the window.
const DATE_ITEMS: &str = "UID INTERNALDATE";

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `UID SEARCH SINCE <date>` in the open mailbox: the UIDs of messages whose
    /// `INTERNALDATE` is on or after `date` (an IMAP `dd-Mon-yyyy` date, RFC 9051 §6.4.4).
    /// `date` is caller-formatted from a calendar date (digits and a fixed month
    /// abbreviation), so it carries no quoting or injection risk.
    pub(crate) async fn uid_search_since(&mut self, date: &str) -> ImapResult<Vec<u32>> {
        let limit = self.negotiated.message_limit();
        let uid_next = self.selected.as_ref().and_then(|open| open.uid_next);
        let fallback = limit.zip(uid_next).zip(imap_day(date));
        // `None` when the server refused the search for its size, as Yahoo documents UID mode
        // doing.
        let found = match self.uid_search(&format!("SINCE {date}")).await {
            Ok(found) => Some(found),
            Err(ImapError::No(detail))
                if fallback.is_some() && opens_with_code(&detail, "LIMIT") =>
            {
                None
            }
            Err(err) => return Err(err),
        };
        let Some(((limit, uid_next), floor)) = fallback else {
            return Ok(found.unwrap_or_default());
        };
        if let Some(found) = found.filter(|found| found.len() < limit) {
            return Ok(found);
        }
        let rows = if self.negotiated.has(Extension::Partial) {
            self.dates_by_page(limit).await?
        } else {
            let mut rows = Vec::new();
            for span in uid_spans(uid_next, limit) {
                rows.extend(self.uid_fetch(&span, DATE_ITEMS).await?);
            }
            rows
        };
        let mut uids: Vec<u32> = rows
            .into_iter()
            .filter(|row| {
                let day = row.internal_date.as_deref().and_then(imap_day);
                day.is_some_and(|day| day >= floor)
            })
            .map(|row| row.uid)
            .collect();
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// Every message's date, `limit` messages per `PARTIAL` page, each page starting above the
    /// highest UID the last one returned. A short page is the last.
    async fn dates_by_page(&mut self, limit: usize) -> ImapResult<Vec<FetchRow>> {
        let mut rows = Vec::new();
        let mut low = 1u32;
        loop {
            let response = self
                .command(&format!(
                    "UID FETCH {low}:* ({DATE_ITEMS}) (PARTIAL 1:{limit})"
                ))
                .await?;
            // `n:*` past the highest UID names the highest message instead (RFC 9051 §6.4.8),
            // which an earlier page already returned; a server may also interleave a row for a
            // message another client changed.
            let page: Vec<FetchRow> = parse::parse_fetch(response.untagged())?
                .into_iter()
                .filter(|row| row.uid >= low)
                .collect();
            let full = page.len() >= limit;
            let Some(highest) = page.iter().map(|row| row.uid).max() else {
                return Ok(rows);
            };
            rows.extend(page);
            if !full {
                return Ok(rows);
            }
            low = highest.saturating_add(1);
        }
    }
}

/// `1..uid_next` as UID spans of at most `limit` UIDs each, so a `FETCH` over one names at
/// most `limit` messages however densely the UIDs are used.
pub(crate) fn uid_spans(uid_next: u32, limit: usize) -> Vec<String> {
    let width = u32::try_from(limit).unwrap_or(u32::MAX).max(1);
    let mut spans = Vec::new();
    let mut low = 1u32;
    while low < uid_next {
        let high = low.saturating_add(width - 1).min(uid_next - 1);
        spans.push(format!("{low}:{high}"));
        low = high.saturating_add(1);
    }
    spans
}

#[cfg(test)]
#[path = "transport_search_tests.rs"]
mod tests;
