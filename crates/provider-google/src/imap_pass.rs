//! A Gmail message sync read over IMAP: All Mail, Spam and Trash as one store
//! ([`provider_imap::GmailMail`]), written as the API pass would write it
//! ([`crate::imap_labels`]).
//!
//! Every pass opens the three folders first and plans from what that says against the cursor
//! (`crate::imap_cursor`):
//!
//! - **Snapshot** (no cursor of this mode, or a folder renumbered, appeared or gone): every message
//!   in the window, newest first, as one reconcile, so whatever the store holds that the server no
//!   longer has is removed at the end.
//! - **Delta**: the messages that arrived (a UID at or above the last `UIDNEXT`), and the state of
//!   every older one changed since the last mod-sequence (`CHANGEDSINCE`). Gmail offers no
//!   `QRESYNC`, so nothing says what was removed; the counts do instead. A folder holding fewer
//!   messages than it held plus what arrived has lost some, and that pass is a reconcile that lists
//!   the ids in the window. A message moved between the three keeps its Gmail id, so it is listed
//!   where it now is and survives the reconcile as the move it was.

use engine_core::{ids::ProviderKey, mail::Message, sync::SyncState, time::CalendarDate};
use engine_provider::{EmailChunk, EmailStream, PassMode, ProviderResult, split_page};
use futures_util::{StreamExt, future::try_join_all, stream};
use provider_imap::{GmailFolder, GmailFolderState, GmailQuery};

use crate::{
    imap_cursor::{FolderMark, ImapCursor, index},
    imap_labels::{LabelIds, key_of, message_of, state_change_of},
    imap_mail::ImapMail,
    named_labels::{Labels, NamedLabels},
    transport::GoogleClient,
};

/// UIDs read per metadata request, newest first.
const BATCH: usize = 500;

/// Metadata requests in flight at once on a first sync. Every IMAP command costs Gmail a round
/// trip (about 200 ms, measured), so a first sync is a run of them, and the account's pool has
/// five connections to overlap them on.
const BATCHES_IN_FLIGHT: usize = 3;

/// The pass `cursor` calls for, streamed.
pub(crate) fn stream<'a>(
    imap: &'a dyn ImapMail,
    client: &'a GoogleClient,
    labels: &'a Labels,
    cursor: Option<&SyncState>,
    floor: Option<CalendarDate>,
    chunk_size: usize,
) -> EmailStream<'a> {
    let previous = cursor.and_then(ImapCursor::read);
    let since = floor.and_then(day_of);
    Box::pin(async_stream::try_stream! {
        let named = labels.resolved(client).await?;
        let ids = LabelIds::read(client).await?;
        let opened_now = try_join_all(GmailFolder::ALL.map(|folder| imap.open(folder))).await?;
        let mut now: [Option<GmailFolderState>; 3] = [None; 3];
        for (folder, state) in GmailFolder::ALL.into_iter().zip(opened_now) {
            now[index(folder)] = state;
        }
        let next = ImapCursor(now.map(|state| state.map(FolderMark::from)));
        let reading = Reading { imap, ids: &ids, named: &named, since };

        match previous.filter(|previous| same_folders(previous, &next)) {
            None => {
                let mut wanted = Vec::new();
                for (folder, state) in opened(&now) {
                    wanted.push((folder, state, reading.window_of(folder, state).await?));
                }
                let total = Some(wanted.iter().map(|(_, _, uids)| uids.len()).sum());
                for (folder, state, uids) in &wanted {
                    let reads: Vec<_> = uids
                        .rchunks(BATCH)
                        .map(|batch| reading.messages(*folder, state.uid_validity, batch))
                        .collect();
                    let mut reads = stream::iter(reads).buffered(BATCHES_IN_FLIGHT);
                    while let Some(messages) = reads.next().await {
                        let messages = messages?;
                        let present = messages.iter().map(|m| m.id.key().clone()).collect();
                        let chunks = split_page(
                            PassMode::Reconcile, messages, Vec::new(), Vec::new(), present, total,
                            chunk_size,
                        );
                        for chunk in chunks {
                            yield chunk;
                        }
                    }
                }
                yield EmailChunk::reconcile_last(Vec::new(), Vec::new(), total, next.state());
            }
            Some(previous) => {
                let mut changed = Vec::new();
                let mut patched = Vec::new();
                let mut lost = false;
                for (folder, state) in opened(&now) {
                    let before = previous.get(folder).expect("same_folders checked it");
                    let validity = state.uid_validity;
                    let arrivals: Vec<u32> = if state.uid_next > before.uid_next {
                        let range = GmailQuery {
                            from_uid: Some(before.uid_next),
                            ..GmailQuery::default()
                        };
                        imap.search(folder, validity, &range).await?
                    } else {
                        Vec::new()
                    };
                    let expected = usize::try_from(before.exists).ok().map(|n| n + arrivals.len());
                    lost |= usize::try_from(state.exists).ok() != expected;
                    // Gmail's mod-sequence is the account's, so it moves in every folder when
                    // anything changes anywhere; a folder holding nothing has nothing to report.
                    let moved = state.highest_modseq > before.highest_modseq;
                    let states = if state.exists > 0 && moved {
                        imap.changed(folder, validity, before.uid_next, before.highest_modseq)
                            .await?
                    } else {
                        Vec::new()
                    };
                    changed.extend(reading.messages(folder, validity, &arrivals).await?);
                    for state in &states {
                        patched.push(state_change_of(state, folder, &ids, &named)?);
                    }
                }
                if lost {
                    let mut present = Vec::new();
                    for (folder, state) in opened(&now) {
                        present.extend(reading.present(folder, state.uid_validity).await?);
                    }
                    let chunks = split_page(
                        PassMode::Reconcile, changed, patched, Vec::new(), present, None,
                        chunk_size,
                    );
                    for chunk in chunks {
                        yield chunk;
                    }
                    yield EmailChunk::reconcile_last(Vec::new(), Vec::new(), None, next.state());
                } else {
                    let chunks = split_page(
                        PassMode::Additive, changed, patched, Vec::new(), Vec::new(), None,
                        chunk_size,
                    );
                    for chunk in chunks {
                        yield chunk;
                    }
                    yield EmailChunk::additive(Vec::new(), Vec::new(), None, next.state());
                }
            }
        }
    })
}

/// What every read of one pass shares.
struct Reading<'a> {
    imap: &'a dyn ImapMail,
    ids: &'a LabelIds,
    named: &'a NamedLabels,
    since: Option<time::Date>,
}

impl Reading<'_> {
    /// The sync window.
    fn window(&self) -> GmailQuery {
        GmailQuery {
            since: self.since,
            ..GmailQuery::default()
        }
    }

    /// The UIDs in the window; nothing for a folder that holds nothing.
    async fn window_of(
        &self,
        folder: GmailFolder,
        state: GmailFolderState,
    ) -> ProviderResult<Vec<u32>> {
        if state.exists == 0 {
            return Ok(Vec::new());
        }
        self.imap
            .search(folder, state.uid_validity, &self.window())
            .await
    }

    /// The messages `uids` name, as the API pass would write them.
    async fn messages(
        &self,
        folder: GmailFolder,
        validity: u32,
        uids: &[u32],
    ) -> ProviderResult<Vec<Message>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let read = self.imap.messages(folder, validity, uids).await?;
        let mut messages = Vec::with_capacity(read.len());
        for message in read {
            messages.push(message_of(message, folder, self.ids, self.named)?);
        }
        Ok(messages)
    }

    /// The keys of every message in the window.
    async fn present(
        &self,
        folder: GmailFolder,
        validity: u32,
    ) -> ProviderResult<Vec<ProviderKey>> {
        let uids = self.imap.search(folder, validity, &self.window()).await?;
        let mut keys = Vec::with_capacity(uids.len());
        for (_, gmail_id) in self.imap.ids(folder, validity, &uids).await? {
            keys.push(key_of(gmail_id)?);
        }
        Ok(keys)
    }
}

/// The folders the account showed, with their counters.
fn opened(now: &[Option<GmailFolderState>; 3]) -> Vec<(GmailFolder, GmailFolderState)> {
    GmailFolder::ALL
        .into_iter()
        .filter_map(|folder| now[index(folder)].map(|state| (folder, state)))
        .collect()
}

/// Whether `previous` describes the same three folders `next` does: each shown or hidden
/// alike, and none renumbered.
fn same_folders(previous: &ImapCursor, next: &ImapCursor) -> bool {
    previous.0.iter().zip(next.0.iter()).all(|pair| match pair {
        (Some(before), Some(now)) => before.uid_validity == now.uid_validity,
        (None, None) => true,
        _ => false,
    })
}

/// A sync window's first day as IMAP's `SINCE` takes it.
fn day_of(date: CalendarDate) -> Option<time::Date> {
    let month = time::Month::try_from(date.month()).ok()?;
    time::Date::from_calendar_date(date.year(), month, date.day()).ok()
}

#[cfg(test)]
#[path = "imap_pass_tests.rs"]
mod tests;
