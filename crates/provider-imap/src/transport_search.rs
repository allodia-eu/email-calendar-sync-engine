//! Finding a sync-depth window's messages when a server will not say how many there are.
//!
//! A server advertising RFC 9738 `MESSAGELIMIT` may answer a search matching more messages
//! than its limit with only some of them. Yahoo answers `UID SEARCH SINCE` with the newest
//! `MESSAGELIMIT` matches by date and a plain `OK`, in either mode, and applies that cut before
//! any other criterion, so neither narrowing by UID nor its `PARTIAL` (`BAD` for any range not
//! starting at 1) reaches the rest. A search that returns as many matches as the limit, or
//! that is refused with `NO [LIMIT]`, is therefore read as cut off, and the window is decided from
//! each message's own `INTERNALDATE`, fetched no more than the limit at a time. A window under the
//! limit, or a server stating none, keeps the single search (`docs/spec-deviations.md`,
//! YAHOO-12 and YAHOO-13).
//!
//! The dates are paged with `PARTIAL` (RFC 9394) where the server offers it: each
//! `UID FETCH <low>:* … (PARTIAL 1:<limit>)` returns the next `limit` messages, so the pages
//! number the messages, not the UIDs. Without it they are UID spans of the limit's width, which
//! on a mailbox whose UIDs run far past its message count costs a command per span.

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    capability::Extension,
    error::{ImapError, ImapResult, opens_with_code},
    parse::{self, FetchRow},
    transport::Connection,
    transport_command::imap_day,
};

/// What the fallback reads of each message: its date, which decides the window.
const DATE_ITEMS: &str = "UID INTERNALDATE";

/// The messages a search names by date: on or after `since`, before `before`, and at or below
/// `highest` UID, each bound optional. Dates are IMAP `dd-Mon-yyyy` days, formatted by the caller
/// from a calendar date (digits and a fixed month abbreviation), so they carry no quoting or
/// injection risk.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DatedSearch<'a> {
    /// `SINCE`: on or after this day.
    pub(crate) since: Option<&'a str>,
    /// `BEFORE`: strictly before this day.
    pub(crate) before: Option<&'a str>,
    /// `UID 1:<highest>`: no message above this UID.
    pub(crate) highest: Option<u32>,
}

impl DatedSearch<'_> {
    /// The `UID SEARCH` criteria. `SINCE` and `BEFORE` compare the server's own `INTERNALDATE`
    /// days (RFC 9051 §6.4.4), so a band ending `BEFORE` a day and a window starting `SINCE` it
    /// are exact complements however the server's zone falls.
    fn criteria(&self) -> String {
        let mut criteria = Vec::new();
        if let Some(highest) = self.highest {
            criteria.push(format!("UID 1:{highest}"));
        }
        if let Some(since) = self.since {
            criteria.push(format!("SINCE {since}"));
        }
        if let Some(before) = self.before {
            criteria.push(format!("BEFORE {before}"));
        }
        if criteria.is_empty() {
            criteria.push("ALL".to_owned());
        }
        criteria.join(" ")
    }

    /// Whether a message the fallback read is named: its UID within `highest`, its date's day
    /// within the two bounds. A bound this search cannot read the day of names nothing.
    fn names(&self, uid: u32, internal_date: Option<&str>) -> bool {
        let Some(day) = internal_date.and_then(imap_day) else {
            return false;
        };
        let on_or_after = |bound: &str| imap_day(bound).is_some_and(|bound| day >= bound);
        let before = |bound: &str| imap_day(bound).is_some_and(|bound| day < bound);
        self.highest.is_none_or(|highest| uid <= highest)
            && self.since.is_none_or(on_or_after)
            && self.before.is_none_or(before)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Connection<S> {
    /// `UID SEARCH SINCE <date>` in the open mailbox: the UIDs of messages whose
    /// `INTERNALDATE` is on or after `date` (an IMAP `dd-Mon-yyyy` date, RFC 9051 §6.4.4).
    pub(crate) async fn uid_search_since(&mut self, date: &str) -> ImapResult<Vec<u32>> {
        self.uid_search_dated(DatedSearch {
            since: Some(date),
            ..DatedSearch::default()
        })
        .await
    }

    /// `UID SEARCH [UID 1:<highest>] [SINCE <since>] [BEFORE <before>]` in the open mailbox,
    /// decided from each message's own `INTERNALDATE` where the server cut the search off at its
    /// message limit (module docs). Both bounds go through the fallback, so a band is the same
    /// band whichever way it was found.
    pub(crate) async fn uid_search_dated(
        &mut self,
        search: DatedSearch<'_>,
    ) -> ImapResult<Vec<u32>> {
        let limit = self.negotiated.message_limit();
        let uid_next = self.selected.as_ref().and_then(|open| open.uid_next);
        let fallback = limit.zip(uid_next);
        // `None` when the server refused the search for its size, as Yahoo documents UID mode
        // doing.
        let found = match self.uid_search(&search.criteria()).await {
            Ok(found) => Some(found),
            Err(ImapError::No(detail))
                if fallback.is_some() && opens_with_code(&detail, "LIMIT") =>
            {
                None
            }
            Err(err) => return Err(err),
        };
        let Some((limit, uid_next)) = fallback else {
            return Ok(found.unwrap_or_default());
        };
        if let Some(found) = found.filter(|found| found.len() < limit) {
            return Ok(found);
        }
        // Only what the search could name is read: nothing at or above `UID 1:<highest>`'s end.
        let span_end = search
            .highest
            .map_or(uid_next, |highest| uid_next.min(highest.saturating_add(1)));
        let rows = if self.negotiated.has(Extension::Partial) {
            self.dates_by_page(limit).await?
        } else {
            let mut rows = Vec::new();
            for span in uid_spans(span_end, limit) {
                rows.extend(self.uid_fetch(&span, DATE_ITEMS).await?);
            }
            rows
        };
        let mut uids: Vec<u32> = rows
            .into_iter()
            .filter(|row| search.names(row.uid, row.internal_date.as_deref()))
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
