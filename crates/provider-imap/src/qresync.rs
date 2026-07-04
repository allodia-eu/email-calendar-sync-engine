//! The QRESYNC incremental delta (RFC 7162) — flag changes and expunges of
//! already-synced mail, reconciled in one round trip.
//!
//! The non-QRESYNC delta in [`crate::sync`] fetches only new arrivals (UIDs at or
//! above the cursor's `UIDNEXT`) and carries no removals, so flag and expunge changes
//! to *already-synced* messages need a periodic snapshot to reconcile. When the
//! session negotiated QRESYNC ([`Connection::negotiate_qresync`]) and the cursor
//! carries a prior `HIGHESTMODSEQ`, this module replaces that delta with a single
//! `UID FETCH 1:* (<items>) (CHANGEDSINCE <modseq> VANISHED)`:
//!
//! - every message whose mod-sequence exceeds the baseline comes back with full
//!   metadata — both genuinely new arrivals and flag-only changes to existing mail —
//!   so the store upserts them by their stable `(mailbox, UIDVALIDITY, UID)` key; and
//! - a `* VANISHED (EARLIER) <set>` lists the UIDs expunged since the baseline, which
//!   become the page's `removed` keys, so the store tombstones them inline.
//!
//! The pass is a **single page** — for periodic sync the changed set is what moved
//! since the last sync, but a bulk server-side change (e.g. "mark all read") returns
//! every changed message in one response, so this does **not** honor the `limit`/paging
//! the snapshot path uses (a documented limitation; paging the delta is a later
//! refinement). It also fetches `1:*` regardless of any sync-depth window, so a flag
//! change to an out-of-window message can re-enter the store — `with_since` (currently
//! provider-only, not host-wired) and the QRESYNC delta are not yet reconciled
//! (`imap-smtp.md`). The new baseline is the SELECT-time `HIGHESTMODSEQ`, already
//! encoded into `next_cursor` by [`crate::sync::sync_page`] before this is called.

use std::cmp::Reverse;

use engine_core::ids::{MailboxId, ProviderKey};
use engine_core::mail::Message;
use engine_core::sync::SyncState;
use engine_provider::{SyncKind, SyncPage};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::ImapResult;
use crate::mail::{message_from_fetch, message_key};
use crate::sync::FETCH_ITEMS;
use crate::transport::Connection;

/// Fetches the QRESYNC delta since `since_modseq` over the bound mailbox: the changed
/// messages (new arrivals + flag changes, full metadata, newest UID first) and the
/// vanished UIDs (expunges) as `removed` keys. `next_cursor` already carries the new
/// `HIGHESTMODSEQ` baseline; `uid_validity` keys the objects.
pub(crate) async fn delta_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    uid_validity: u32,
    next_cursor: SyncState,
    since_modseq: u64,
) -> ImapResult<SyncPage<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (mut rows, vanished) = conn
        .uid_fetch_changedsince("1:*", FETCH_ITEMS, since_modseq)
        .await?;
    // Keep only the **solicited** full rows. Once CONDSTORE is enabled the server may
    // interleave an *unsolicited* flag-only `* n FETCH (UID x FLAGS (..) MODSEQ (..))`
    // — no ENVELOPE — for a message whose flags another client changed mid-fetch
    // (RFC 7162 §3.2). Mapping such a row would build an empty-envelope `Message` that
    // the store upserts as a full-payload replace, wiping a good message's
    // subject/from/date/size. We requested ENVELOPE, so every row we asked for carries
    // one; a row without it is the unsolicited notification — drop it. The change it
    // signals rides a later `CHANGEDSINCE` once its new mod-sequence is the baseline.
    rows.retain(|row| row.envelope.is_some());
    // Newest UID first, so a streaming host renders the most recent changes first —
    // the same ordering the snapshot/new-arrivals paths use.
    rows.sort_unstable_by_key(|row| Reverse(row.uid));
    let changed: Vec<Message> = rows
        .iter()
        .map(|row| message_from_fetch(row, mailbox, uid_validity))
        .collect();
    let removed: Vec<ProviderKey> = vanished
        .iter()
        .map(|&uid| message_key(mailbox.as_str(), uid_validity, uid))
        .collect();
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        removed,
        present: Vec::new(),
        next_page: None,
        next_cursor,
        total: None,
    })
}

#[cfg(test)]
#[path = "qresync_tests.rs"]
mod tests;
