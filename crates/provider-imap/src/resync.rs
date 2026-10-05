//! The delta of a session without QRESYNC: what changed in the mail already synced, read the
//! way RFC 4549 §4.3 describes for a disconnected client.
//!
//! Without `CHANGEDSINCE`/`VANISHED` the server cannot say what moved, so this asks for the
//! current flags of every message below the prior `UIDNEXT` (one short line each) and fetches
//! the arrivals at or above it whole. The flags become [`MailStateChange`]s, which the store
//! writes to the state columns of the rows it holds and ignores for any it does not. Everything
//! the two halves returned is the page's `present` set, so the pass **reconciles**: a stored
//! message the server no longer returned (expunged, or moved by this or another client) is
//! tombstoned at the end of the pass.
//!
//! A sync-depth window bounds both halves through one `UID SEARCH SINCE`, so mail outside it is
//! neither fetched nor counted present. Each `UID FETCH` names at most `limit` UIDs, because a
//! server may cap how many messages one command touches (RFC 9738 `MESSAGELIMIT`). Like the
//! QRESYNC delta, the pass is one page.

use std::cmp::Reverse;

use engine_core::{
    ids::{MailboxId, ProviderKey},
    mail::{MailStateChange, Message},
    sync::SyncState,
};
use engine_provider::{SyncKind, SyncPage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    error::ImapResult,
    mail::{message_from_fetch, message_key},
    parse::FetchRow,
    qresync::state_change,
    sync::{FETCH_ITEMS, uid_set_spec},
    transport::Connection,
};

/// The `FETCH` items for a message already stored: its identity and its whole flag set.
const STATE_ITEMS: &str = "UID FLAGS";

/// Builds the reconciling delta page for the bound mailbox. `synced_below` is the prior
/// cursor's `UIDNEXT`, `uid_next` the one this `SELECT` reported, `since` the window floor if
/// any, and `limit` the most UIDs one `FETCH` may name (`0` for no cap).
#[allow(clippy::too_many_arguments)] // the post-SELECT page inputs are all distinct
pub(crate) async fn resync_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    uid_validity: u32,
    next_cursor: SyncState,
    synced_below: u32,
    uid_next: u32,
    since: Option<&str>,
    limit: usize,
) -> ImapResult<SyncPage<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (held, arriving) = match since {
        Some(date) => {
            let mut in_window = conn.uid_search_since(date).await?;
            in_window.sort_unstable();
            let split = in_window.partition_point(|&uid| uid < synced_below);
            let arriving: Vec<u32> = in_window[split..]
                .iter()
                .copied()
                .filter(|&uid| uid < uid_next)
                .collect();
            (
                listed_sets(&in_window[..split], limit),
                listed_sets(&arriving, limit),
            )
        }
        None => (
            range_sets(1, synced_below, limit),
            range_sets(synced_below, uid_next, limit),
        ),
    };

    let mut patched: Vec<MailStateChange> = Vec::new();
    let mut present: Vec<ProviderKey> = Vec::new();
    for set in &held {
        for row in conn.uid_fetch(set, STATE_ITEMS).await? {
            // A bounded set returns only what it names, but a server may interleave an
            // unsolicited row for a message another client changed; one above the
            // watermark is an arrival, fetched whole below.
            if row.uid < synced_below {
                present.push(key_of(&row, mailbox, uid_validity));
                patched.push(state_change(&row, mailbox, uid_validity));
            }
        }
    }

    let mut arrivals: Vec<FetchRow> = Vec::new();
    for set in &arriving {
        arrivals.extend(conn.uid_fetch(set, FETCH_ITEMS).await?);
    }
    // We asked for `ENVELOPE`, so a solicited row has one; anything else is an unsolicited
    // flag-only row, not a message.
    arrivals.retain(|row| row.envelope.is_some() && row.uid >= synced_below);
    arrivals.sort_unstable_by_key(|row| Reverse(row.uid));
    let changed: Vec<Message> = arrivals
        .iter()
        .map(|row| message_from_fetch(row, mailbox, uid_validity))
        .collect();
    present.extend(changed.iter().map(|message| message.id.key().clone()));

    Ok(SyncPage {
        kind: SyncKind::Snapshot,
        total: Some(changed.len()),
        changed,
        patched,
        removed: Vec::new(),
        present,
        next_page: None,
        next_cursor,
    })
}

fn key_of(row: &FetchRow, mailbox: &MailboxId, uid_validity: u32) -> ProviderKey {
    message_key(mailbox.as_str(), uid_validity, row.uid)
}

/// `low..high` as UID ranges of at most `limit` UIDs each (`0` = one range). A range can hold
/// no more messages than UIDs, so this respects a message cap whatever the gaps.
fn range_sets(low: u32, high: u32, limit: usize) -> Vec<String> {
    if high <= low {
        return Vec::new();
    }
    let step = u32::try_from(limit)
        .ok()
        .filter(|&step| step > 0)
        .unwrap_or(high - low);
    (low..high)
        .step_by(step as usize)
        .map(|start| format!("{start}:{}", (start + step - 1).min(high - 1)))
        .collect()
}

/// Ascending `uids` as compact sets of at most `limit` UIDs each (`0` = one set).
fn listed_sets(uids: &[u32], limit: usize) -> Vec<String> {
    if uids.is_empty() {
        return Vec::new();
    }
    let step = if limit == 0 { uids.len() } else { limit };
    uids.chunks(step).map(uid_set_spec).collect()
}
