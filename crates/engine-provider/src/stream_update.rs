//! How one [`EmailChunk`] becomes the update a store applies: the window and patch rules for a
//! host that applies [`Provider::stream_email`](crate::Provider::stream_email) chunk by chunk.
//!
//! They live beside the chunk rather than in one orchestrator so that every such host
//! (`engine-sync`'s streaming loop, or a host that keeps its own store) reaches the same
//! conclusions from the same code (`store-and-sync.md`). The default
//! [`Provider::sync_email`](crate::Provider::sync_email) is not one of them: it drains a whole
//! pass into one update and is handed no window, so it filters nothing.

use std::collections::BTreeSet;

use engine_core::{
    ids::ProviderKey,
    mail::Message,
    sync::{MailboxWindows, SyncState, SyncUpdate},
};

use crate::{EmailChunk, PassMode};

impl EmailChunk {
    /// Turns this chunk into the [`SyncUpdate`] to apply and the cursor to advance to once it is
    /// applied (`None`: hold the cursor).
    ///
    /// - [`PassMode::Additive`]: a delta (upserts plus explicit removals), advancing to the chunk's
    ///   checkpoint. Arrivals `window` does not admit are dropped.
    /// - [`PassMode::Reconcile`]: an intermediate chunk applies additively and holds the cursor
    ///   while `present` accumulates the keys it covers; the final chunk applies a snapshot against
    ///   the whole accumulated set (tombstoning), takes it out of `present`, and advances the
    ///   cursor.
    ///
    /// `present` belongs to one pass: start each pass with an empty set and hand every chunk of
    /// that pass the same one.
    ///
    /// The chunk's state changes go **into the update** rather than beside it: a key that also
    /// arrived whole drops its patch (the object is the later word), and a snapshot carries no
    /// partials at all, because it is already the scope's whole current state.
    #[must_use]
    pub fn into_update(
        self,
        window: &MailboxWindows,
        present: &mut BTreeSet<ProviderKey>,
    ) -> (SyncUpdate<Message>, Option<SyncState>) {
        let Self {
            mode,
            changed,
            patched,
            removed,
            present: page_present,
            advance_to,
            ..
        } = self;
        let update = match mode {
            // A delta's arrivals are new to *us* rather than recent — every protocol reports a
            // message filed into a folder as one, and none of them can carry a date filter — so
            // without this, mail the user asked not to keep walks back in and no later pass
            // removes it. This is the only place that can say no: a delta has no present set, so
            // nothing downstream ever revisits the decision.
            PassMode::Additive => SyncUpdate::delta(admitted(changed, window), removed),
            // A reconcile is **not** filtered here, and that is deliberate. Its `present` set is
            // the adapter's own enumeration of the window, and the store tombstones by diffing
            // against it, so the adapter already decided — with its server's date semantics,
            // which are not ours. Re-deciding with a second rule can only disagree: IMAP `SINCE`
            // compares `INTERNALDATE` in the *server's* timezone, so a message a snapshot
            // deliberately asked for can read as out-of-window here and be dropped while its key
            // stays in `present`. Depth on the snapshot path is the adapter's job.
            PassMode::Reconcile => {
                present.extend(page_present);
                if advance_to.is_some() {
                    SyncUpdate::snapshot(changed, core::mem::take(present))
                } else {
                    SyncUpdate::delta(changed, removed)
                }
            }
        };
        (update.with_patched(patched), advance_to)
    }
}

/// The messages a window admits, by the mailboxes they are filed in and the date a store sorts
/// and filters them on.
///
/// A message in a deepened mailbox is judged by that mailbox's window, so a pass that fetched a
/// folder further back keeps what it fetched: a fresh IMAP backfill commits its groups as
/// additive chunks before its completing reconcile, and those chunks pass through here.
fn admitted(changed: Vec<Message>, window: &MailboxWindows) -> Vec<Message> {
    if !window.account().is_bounded() {
        return changed;
    }
    changed
        .into_iter()
        .filter(|message| {
            window.admits(
                message.mailboxes.iter(),
                message.received_at.or(message.sent_at),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "stream_update_tests.rs"]
mod tests;
