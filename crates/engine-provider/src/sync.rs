//! The per-scope sync result an adapter returns.

use engine_core::sync::{SyncObject, SyncState, SyncUpdate};

/// One scope's worth of fetched changes plus the cursor to persist after applying
/// them.
///
/// The [`update`](ScopeSync::update) is a delta or a snapshot (the adapter signals
/// which through [`SyncUpdate`] itself); [`next_cursor`](ScopeSync::next_cursor) is
/// the opaque provider state to advance to. The engine round-trips the cursor
/// without parsing it (`store-and-sync.md`) and carries it straight into the
/// store's `ApplyBatch::next_state`.
///
/// `T` is the scope's normalized object type (a `Mailbox`, `Message`, `Calendar`,
/// or `Event`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSync<T: SyncObject> {
    /// The normalized changes for the scope (delta or snapshot).
    pub update: SyncUpdate<T>,
    /// The provider cursor to persist once `update` is applied.
    pub next_cursor: SyncState,
}

impl<T: SyncObject> ScopeSync<T> {
    /// Pairs an update with the cursor to advance to after applying it.
    #[must_use]
    pub fn new(update: SyncUpdate<T>, next_cursor: SyncState) -> Self {
        Self {
            update,
            next_cursor,
        }
    }

    /// Whether this result is a full/bounded snapshot (so the store must tombstone
    /// local rows absent from it) rather than a delta.
    #[must_use]
    pub fn is_snapshot(&self) -> bool {
        self.update.is_snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use engine_core::{
        ids::{MailboxId, MessageId},
        mail::Message,
        membership::Memberships,
    };

    use super::*;

    fn message(id: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from("inbox").unwrap()),
        )
    }

    #[test]
    fn snapshot_flag_follows_the_update() {
        let delta: ScopeSync<Message> = ScopeSync::new(
            SyncUpdate::delta(vec![message("m1")], vec![]),
            SyncState::new("s1"),
        );
        assert!(!delta.is_snapshot());
        assert_eq!(delta.next_cursor.as_str(), "s1");

        let snapshot: ScopeSync<Message> = ScopeSync::new(
            SyncUpdate::snapshot(vec![message("m1")], BTreeSet::new()),
            SyncState::new("s2"),
        );
        assert!(snapshot.is_snapshot());
    }
}
