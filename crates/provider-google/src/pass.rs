//! The passes a Gmail message sync runs, by where its cursor stands (`crate::cursor`).
//!
//! - **Delta** (a synced cursor): `history.list` from the stored historyId, additively.
//! - **Backfill** (no cursor, or one part way): the window fetched a page at a time, each page
//!   checkpointed with the token of the next, and a delta from the cursor's historyId before every
//!   page but a fresh backfill's first, so mail arriving during a backfill of hours is not held
//!   back until it ends. Additive throughout: nothing is tombstoned, so a pass may stop anywhere.
//! - **Reconcile** (every page fetched): the window's ids listed and nothing else, so the store
//!   tombstones what the server no longer has. A listing costs 5 units for up to 500 ids and a
//!   message 20 (`crate::quota`), so this costs a fraction of the backfill, and it is a pass of its
//!   own because a pass is one mode throughout (`engine_provider::PassMode`).
//!
//! A backfill is how a first sync runs, and how a sync restarts once Gmail has aged the stored
//! historyId out of its window.

use engine_core::{error::FailureClass, ids::ProviderKey, sync::SyncState, time::CalendarDate};
use engine_provider::{EmailChunk, EmailStream, PageToken, PassMode, split_page};
use serde_json::Value;

use crate::{
    cursor::GmailCursor,
    error::GoogleError,
    fetch,
    json::{opt_str, req_str},
    named_labels::Labels,
    transport::GoogleClient,
};

/// Ids per listing in the reconcile: Gmail's largest page, at the same 5 units as any other.
const RECONCILE_PAGE: u16 = 500;

/// The pass `cursor` calls for, streamed.
pub(crate) fn stream<'a>(
    client: &'a GoogleClient,
    labels: &'a Labels,
    cursor: Option<&SyncState>,
    floor: Option<CalendarDate>,
    chunk_size: usize,
) -> EmailStream<'a> {
    match cursor.and_then(GmailCursor::read) {
        Some(GmailCursor::Reconcile { history }) => reconcile(client, history, floor),
        start => additive(client, labels, start, floor, chunk_size),
    }
}

/// A delta, or a backfill carried on from where `start` left it.
fn additive<'a>(
    client: &'a GoogleClient,
    labels: &'a Labels,
    start: Option<GmailCursor>,
    floor: Option<CalendarDate>,
    chunk_size: usize,
) -> EmailStream<'a> {
    Box::pin(async_stream::try_stream! {
        let named = labels.resolved(client).await?;
        // `backfill` is the page the listing goes on from, `Some(None)` its first, and `None`
        // for a pass that is a delta and nothing more. A historyId captured a moment ago has
        // nothing to report yet, so the first page of a fresh backfill skips its delta.
        let (mut history, mut backfill, mut skip_delta) = match start {
            Some(GmailCursor::Delta { history }) => (history, None, false),
            Some(GmailCursor::Backfill { history, page }) => (history, Some(page), false),
            _ => (fetch::current_history_id(client).await?, Some(None), true),
        };
        loop {
            let caught_up_from = history.clone();
            let mut history_page: Option<PageToken> = None;
            loop {
                if skip_delta {
                    break;
                }
                let page = match fetch::delta_page(client, &history, history_page.as_ref(), &named)
                    .await
                {
                    Ok(page) => page,
                    // Gmail has aged the historyId out of its window, so what changed since
                    // cannot be known: the window is fetched again from the top, and the
                    // reconcile at its end removes what is gone. Only before a history page has
                    // been passed on, so no change is applied twice from two cursors.
                    Err(err)
                        if history_page.is_none()
                            && err.failure_class() == FailureClass::NeedsResync =>
                    {
                        history = fetch::current_history_id(client).await?;
                        backfill = Some(None);
                        break;
                    }
                    Err(err) => Err(err)?,
                };
                let next_page = page.next_page;
                let next_history = page.next_cursor;
                let chunks = split_page(
                    PassMode::Additive,
                    page.changed,
                    page.patched,
                    page.removed,
                    Vec::new(),
                    None,
                    chunk_size,
                );
                for chunk in chunks {
                    yield chunk;
                }
                if let Some(next) = next_page {
                    history_page = Some(next);
                } else {
                    history = next_history;
                    break;
                }
            }
            skip_delta = false;

            let Some(page) = backfill.clone() else {
                yield checkpoint(&GmailCursor::Delta { history });
                break;
            };
            if history != caught_up_from {
                yield checkpoint(&GmailCursor::Backfill {
                    history: history.clone(),
                    page: page.clone(),
                });
            }

            let fetch::ListedPage {
                messages,
                next_page: next,
                stopped,
            } = match fetch::listed_page(client, page.as_ref(), floor, &named).await {
                Ok(listed) => listed,
                // A token Gmail can no longer read; one it merely does not recognise answers
                // the first page by itself (`invalid_page_token.json`).
                Err(GoogleError::Status { status: 400, .. }) if page.is_some() => {
                    backfill = Some(None);
                    continue;
                }
                Err(err) => Err(err)?,
            };
            let chunks = split_page(
                PassMode::Additive,
                messages,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                None,
                chunk_size,
            );
            for chunk in chunks {
                yield chunk;
            }
            // The page is short: what it fetched is passed on, and the checkpoint stays before it.
            if let Some(err) = stopped {
                Err(err)?;
            }
            if let Some(next) = next {
                yield checkpoint(&GmailCursor::Backfill {
                    history: history.clone(),
                    page: Some(next.clone()),
                });
                backfill = Some(Some(next));
            } else {
                yield checkpoint(&GmailCursor::Reconcile { history });
                break;
            }
        }
    })
}

/// The window's ids, listed and not fetched, ending on `history`.
fn reconcile(
    client: &GoogleClient,
    history: SyncState,
    floor: Option<CalendarDate>,
) -> EmailStream<'_> {
    Box::pin(async_stream::try_stream! {
        let mut page: Option<PageToken> = None;
        loop {
            let doc = client
                .get(&fetch::list_url(client, page.as_ref(), floor, RECONCILE_PAGE))
                .await?;
            let present = listed_ids(&doc)?;
            if let Some(next) = opt_str(&doc, "nextPageToken") {
                yield EmailChunk::reconcile_page(Vec::new(), present, None);
                page = Some(PageToken::new(next));
            } else {
                let done = GmailCursor::Delta { history }.state();
                yield EmailChunk::reconcile_last(Vec::new(), present, None, done);
                break;
            }
        }
    })
}

/// An empty chunk that moves the cursor to `reached`.
fn checkpoint(reached: &GmailCursor) -> EmailChunk {
    EmailChunk::additive(Vec::new(), Vec::new(), None, reached.state())
}

/// The message ids of one `messages.list` page; none on an empty mailbox.
fn listed_ids(doc: &Value) -> Result<Vec<ProviderKey>, GoogleError> {
    let mut ids = Vec::new();
    for entry in doc
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = req_str(entry, "id")?;
        ids.push(
            ProviderKey::new(id)
                .map_err(|e| GoogleError::protocol(format!("bad listed id: {e}")))?,
        );
    }
    Ok(ids)
}

#[cfg(test)]
#[path = "pass_tests.rs"]
mod tests;
