//! A widening's bands over JMAP's one `Email` scope: only the mail a wider window adds, walked
//! without giving up the `Email` state (`docs/design/windows-and-widening.md`, §4.2).
//!
//! The bands become one `Email/query` filter ([`band_filter`]), walked by anchor as a snapshot is
//! ([`Walk`], in [`Order::Fixed`] on `receivedAt`) and closed by a listing of the ids. Each page is
//! committed as [`PassMode::Additive`](engine_provider::PassMode::Additive) chunks that carry no
//! `present` set and tombstone nothing.
//!
//! **The cursor keeps the `Email` state it was handed.** That state predates the walk, so any
//! change made to a band message during or after the walk is replayed by the next
//! `Email/changes`; a message moved into a band mailbox afterwards keeps its id and arrives that
//! way too.
//!
//! **The walk's place is stored in the cursor**, beside the kept state, after every page: the
//! last ids the walk read ([`Position`]). When the caller says the widening resumes
//! ([`Widening::resumes`]), the band continues after them ([`Walk::resumed`]) instead of walking
//! the band again, stepping back through the ids when the last was destroyed, and starting over
//! when none is left (keyed upserts: time, not correctness). Its closing listing fetches only what
//! the query places after them and the resumed walk did not see: `receivedAt` is a property no
//! update changes, so an id placed at or before them was read before the kill. Otherwise the
//! place belongs to another widening and is ignored: only the caller knows whether a pass under a
//! narrower window ran in between and dropped mail the walk had not reached. A widening that
//! resumes toward a later floor (a window computed from today) walks a narrower filter with the
//! same order and upper end, so the ids it resumes after still mark what was read.
//!
//! The position rides through a delta that tombstones nothing ([`carried`]) and is dropped by a
//! snapshot, which may have tombstoned what the band stored; a resumed band without one walks
//! from the top.

use engine_core::{
    ids::ProviderKey,
    sync::{SyncState, Widening},
};
use engine_provider::{EmailChunk, EmailStream, PassMode, split_page};
use serde_json::json;

use crate::{
    JmapProvider,
    email_window::band_filter,
    fetch::MemberFetch,
    mail::{EMAIL_PROPERTIES, message_from_json},
    request::capability,
    sync_ops::clamp_limit,
    walk::{Order, Walk},
};

/// How a stored band position starts, so it can never be read as a plain `Email` state.
const BAND: &str = "band:";
/// How many of the last ids read a position keeps to resume after.
const KEPT_IDS: usize = 8;

/// Where an interrupted band walk had got to: the last ids it read, in query order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Position {
    read: Vec<ProviderKey>,
}

/// A stored cursor's `Email` state, and the band position beside it, if any.
pub(crate) fn split(cursor: &SyncState) -> (SyncState, Option<Position>) {
    let parsed = cursor.as_str().strip_prefix(BAND).and_then(|rest| {
        let (read, state) = rest.split_once('\n')?;
        let read = read
            .split(' ')
            .filter(|id| !id.is_empty())
            .map(|id| ProviderKey::new(id).ok())
            .collect::<Option<Vec<_>>>()?;
        Some((SyncState::new(state), Position { read }))
    });
    match parsed {
        Some((state, position)) => (state, Some(position)),
        None => (cursor.clone(), None),
    }
}

/// `state` with `position` stored beside it: `band:<ids>\n<state>`. An id is URL-safe base64
/// (RFC 8620 §1.2), with no space or line break in it.
fn join(state: &SyncState, position: &Position) -> SyncState {
    let read: Vec<&str> = position.read.iter().map(ProviderKey::as_str).collect();
    SyncState::new(format!("{BAND}{}\n{}", read.join(" "), state.as_str()))
}

/// `next`, the state a delta that tombstoned nothing ends on, with `position` carried, so the band
/// it records still continues.
pub(crate) fn carried(next: SyncState, position: Option<&Position>) -> SyncState {
    match position {
        Some(position) => join(&next, position),
        None => next,
    }
}

impl JmapProvider {
    /// Streams the bands of `widening`, keeping the `Email` state in `cursor`. See the module docs.
    pub(crate) fn band_stream<'a>(
        &'a self,
        cursor: &'a SyncState,
        widening: &'a Widening,
        fetch_batch: usize,
        chunk_size: usize,
    ) -> EmailStream<'a> {
        let (kept, recorded) = split(cursor);
        let filter = band_filter(&widening.bands());
        Box::pin(async_stream::try_stream! {
            // A widening always has a band with an upper end, so there is always a filter.
            let Some(filter) = filter else {
                yield EmailChunk::additive(Vec::new(), Vec::new(), Some(0), kept);
                return;
            };
            let account = self.mail_account()?;
            let fetch = MemberFetch {
                executor: self.executor.as_ref(),
                account: &account,
                using: &[capability::CORE, capability::MAIL],
                type_name: "Email",
                properties: Some(EMAIL_PROPERTIES),
            };
            let limit = clamp_limit(
                fetch_batch,
                fetch.executor.session().limits().max_objects_in_get,
            );
            let sort = json!([{ "property": "receivedAt", "isAscending": false }]);
            let mut walk = Walk::new(&fetch, Some(sort), Some(filter), Order::Fixed, limit);
            // The caller decides whether this band continues the place the cursor recorded.
            if let Some(position) = recorded.filter(|_| widening.resumes()) {
                walk = walk.resumed(position.read);
            }
            loop {
                let mut page = walk.next(message_from_json).await?;
                let done = walk.is_done();
                let advance_to = if done {
                    let missed = walk.settle(message_from_json).await?;
                    page.changed.extend(missed.changed);
                    page.total = missed.total.or(page.total);
                    kept.clone()
                } else {
                    let position = Position {
                        read: walk.tail(KEPT_IDS),
                    };
                    join(&kept, &position)
                };
                for chunk in page_chunks(page.changed, page.total, chunk_size, advance_to) {
                    yield chunk;
                }
                if done {
                    break;
                }
            }
        })
    }
}

/// One page as additive chunks, the last of which advances to `advance_to`.
fn page_chunks(
    changed: Vec<engine_core::mail::Message>,
    total: Option<usize>,
    chunk_size: usize,
    advance_to: SyncState,
) -> Vec<EmailChunk> {
    let mut chunks = split_page(
        PassMode::Additive,
        changed,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        total,
        chunk_size,
    );
    match chunks.last_mut() {
        Some(last) => last.advance_to = Some(advance_to),
        None => chunks.push(EmailChunk::additive(
            Vec::new(),
            Vec::new(),
            total,
            advance_to,
        )),
    }
    chunks
}

#[cfg(test)]
#[path = "email_band_tests.rs"]
mod tests;
