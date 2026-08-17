//! The QRESYNC incremental delta (RFC 7162) — flag changes and expunges of
//! already-synced mail, reconciled without re-downloading their metadata.
//!
//! The non-QRESYNC delta in [`crate::sync`] fetches only new arrivals (UIDs at or
//! above the cursor's `UIDNEXT`) and carries no removals, so flag and expunge changes
//! to *already-synced* messages need a periodic snapshot to reconcile. When the
//! session negotiated QRESYNC ([`Connection::negotiate_qresync`]) and the cursor
//! carries a prior `HIGHESTMODSEQ`, this module replaces that delta.
//!
//! The prior cursor's `UIDNEXT` splits the UID space, and the two halves are worth
//! different amounts of network:
//!
//! - **below it** the message is already stored, and its content cannot have moved — IMAP has no
//!   in-place edit, so an edit or a move mints a new UID. `FLAGS` is therefore the whole of what a
//!   `CHANGEDSINCE` row there can be reporting, and each becomes a [`MailStateChange`] the store
//!   applies to that row's state columns. This is the half a "mark all read" lands in: it used to
//!   return every changed message's `ENVELOPE` and `BODYSTRUCTURE` to write a flag bitfield.
//! - **at or above it** the message is new to us, so it comes back with the full metadata a first
//!   sync needs, as `changed`.
//!
//! `* VANISHED (EARLIER) <set>` rides the first command, whose range is exactly the
//! space an expunge can remove something we hold from; those UIDs become the page's
//! `removed` keys, so the store tombstones them inline.
//!
//! The pass is a **single page** — for periodic sync the changed set is what moved
//! since the last sync, but a bulk server-side change returns every changed message in
//! one response, so this does **not** honor the `limit`/paging the snapshot path uses
//! (a documented limitation; paging the delta is a later refinement). It also fetches
//! from UID 1 regardless of any sync-depth window; a state change for an out-of-window
//! message is now harmless, because applying one is an `UPDATE` that matches no row,
//! but a new arrival still enters unwindowed (`imap-smtp.md`). The new baseline is the
//! SELECT-time `HIGHESTMODSEQ`, already encoded into `next_cursor` by
//! [`crate::sync::sync_page_selected`] before this is called.

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
    mail::{flags_to_keywords, message_from_fetch, message_key},
    parse::FetchRow,
    sync::FETCH_ITEMS,
    transport::Connection,
};

/// The `FETCH` items a state-only row needs: the identity and the whole flag set.
///
/// `FLAGS` is the complete set, not a diff, which is what makes the resulting
/// [`MailStateChange`] idempotent on replay.
const STATE_ITEMS: &str = "UID FLAGS";

/// Fetches the QRESYNC delta since `since_modseq` over the bound mailbox: state changes
/// for already-synced mail, full metadata for new arrivals, and the vanished UIDs
/// (expunges) as `removed` keys.
///
/// `synced_below` is the **prior** cursor's `UIDNEXT` — every UID under it was already
/// synced — and `uid_next` the one this `SELECT` reported. `next_cursor` already carries
/// the new `HIGHESTMODSEQ` baseline; `uid_validity` keys the objects.
pub(crate) async fn delta_page<S>(
    conn: &mut Connection<S>,
    mailbox: &MailboxId,
    uid_validity: u32,
    next_cursor: SyncState,
    since_modseq: u64,
    synced_below: u32,
    uid_next: u32,
) -> ImapResult<SyncPage<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    // Nothing was synced below UID 1, so there is no state half and nothing an expunge
    // could remove — the whole mailbox is new arrivals.
    let (state_rows, vanished) = if synced_below > 1 {
        conn.uid_fetch_changedsince(
            &format!("1:{}", synced_below - 1),
            STATE_ITEMS,
            since_modseq,
        )
        .await?
    } else {
        (Vec::new(), Vec::new())
    };

    // Every UID at or above the prior UIDNEXT was assigned after the baseline, so all of
    // them are changed and `CHANGEDSINCE` would only restate that. Guarded on the current
    // UIDNEXT because `n:*` matches the *highest* UID when nothing reaches `n`
    // (RFC 9051 §6.4.8) — unguarded, an idle mailbox would re-fetch its newest message
    // as an arrival on every sync.
    let mut arrivals = if uid_next > synced_below {
        conn.uid_fetch(&format!("{synced_below}:*"), FETCH_ITEMS)
            .await?
    } else {
        Vec::new()
    };
    // Newest UID first, so a streaming host renders the most recent arrivals first —
    // the same ordering the snapshot/new-arrivals paths use.
    arrivals.sort_unstable_by_key(|row| Reverse(row.uid));

    let mut changed = Vec::new();
    let mut patched: Vec<MailStateChange> = Vec::new();
    for row in &arrivals {
        // We asked for `ENVELOPE`, so a solicited row carries one. A row without it is
        // an *unsolicited* flag-only `* n FETCH (UID x FLAGS (..))` the server may
        // interleave once CONDSTORE is on, for a message another client changed
        // mid-fetch (RFC 7162 §3.2). Mapping it as a message would build an
        // empty-envelope object that overwrites good metadata — but it is a perfectly
        // good state change, which is what the flags it carries were announcing.
        //
        // The UID floor is the same `n:*` trap the guard above covers, in the case the
        // guard cannot see: UIDNEXT can have moved while *nothing* at or above
        // `synced_below` survives (everything that arrived since the baseline was also
        // expunged), and `synced_below:*` then answers with the newest **already-synced**
        // message. Its content cannot have moved — IMAP has no in-place edit — so taking
        // it as an arrival would rewrite a payload this pass had no reason to touch.
        if row.envelope.is_some() && row.uid >= synced_below {
            changed.push(message_from_fetch(row, mailbox, uid_validity));
        } else {
            patched.push(state_change(row, mailbox, uid_validity));
        }
    }
    patched.extend(
        state_rows
            .iter()
            .map(|row| state_change(row, mailbox, uid_validity)),
    );

    let removed: Vec<ProviderKey> = vanished
        .iter()
        .map(|&uid| message_key(mailbox.as_str(), uid_validity, uid))
        .collect();
    Ok(SyncPage {
        kind: SyncKind::Delta,
        changed,
        patched,
        removed,
        present: Vec::new(),
        next_page: None,
        next_cursor,
        total: None,
    })
}

/// The state change one `FLAGS` row reports.
///
/// It carries the keyword set alone, which is what an IMAP message stores today. That is a
/// **gap, not a protocol fact**: RFC 7162 §3.1.2 defines a per-message `MODSEQ`, and §3.1.4.1
/// says a CONDSTORE-enabled session returns it on every untagged `FETCH` whether or not it was
/// asked for — so these rows already carry one and this adapter simply does not parse it
/// ([`FetchRow`] has no field for it). Storing it is the prerequisite for `STORE
/// (UNCHANGEDSINCE …)`, the conditional write that would give IMAP mail the write guard no
/// provider has yet; it belongs with that work, not here. IMAP has no per-message modification
/// *time* either way.
///
/// Until then the change is silent about the token rather than claiming it is absent, and the
/// store keeps whatever it holds instead of blanking it
/// ([`RevisionTokens::or`](engine_core::version::RevisionTokens::or)).
fn state_change(row: &FetchRow, mailbox: &MailboxId, uid_validity: u32) -> MailStateChange {
    MailStateChange::keywords(
        message_key(mailbox.as_str(), uid_validity, row.uid),
        flags_to_keywords(&row.flags),
    )
}

#[cfg(test)]
#[path = "qresync_tests.rs"]
mod tests;
