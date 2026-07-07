//! The streaming sync observer: progress **and** the exact rows that changed.
//!
//! A host driving a streamed sync wants two things after each commit: live
//! "downloaded Y of X" progress, and the specific messages that just landed so it
//! can splice them into its view **without re-querying the whole mailbox**. A
//! [`SyncObserver`] receives both in one [`SyncCommit`], reported after every
//! committed chunk. The commit borrows the just-committed data (zero-copy on the
//! engine side); a host clones only what it keeps.

use engine_core::{ids::ProviderKey, mail::Message, sync::SyncScope};

/// One durable commit the streaming sync just made — progress plus the rows it
/// changed, borrowed for the duration of the callback.
///
/// `upserted` and `removed` describe **this commit only** (not the running set), so
/// a host applies them incrementally: insert/update the `upserted` messages, splice
/// out the `removed` keys. `fetched`/`total` are the running progress for the pass.
#[derive(Debug)]
#[non_exhaustive]
pub struct SyncCommit<'a> {
    /// The scope this commit belongs to.
    pub scope: &'a SyncScope,
    /// Objects committed (and so host-visible) so far this pass — the running count.
    pub fetched: usize,
    /// The provider's total for the pass, if known (the `X` in "Y of X").
    pub total: Option<usize>,
    /// Messages created or updated in **this** commit — ready to render, no re-read.
    pub upserted: &'a [Message],
    /// Keys explicitly removed in **this** commit (a delta's destroyed ids, a
    /// QRESYNC `VANISHED` set) — ready to splice out.
    ///
    /// A [`PassMode::Reconcile`](engine_provider::PassMode) pass tombstones absent
    /// rows by a present-set diff computed inside the store, so those keys are **not**
    /// enumerated here — [`tombstoned`](Self::tombstoned) instead reports *how many*
    /// were removed, and a host reconciles by re-reading the scope. The common
    /// incremental paths (delta, `VANISHED`) do carry their keys here.
    pub removed: &'a [ProviderKey],
    /// Rows removed by this commit that are **not** enumerated in `removed` — the
    /// present-set tombstones of a reconcile pass. `> 0` tells a host a re-snapshot
    /// dropped rows it cannot name individually, so it should re-read the scope; `0`
    /// on every incremental (delta/`VANISHED`) commit, whose removals are in `removed`.
    pub tombstoned: usize,
}

impl SyncCommit<'_> {
    /// Whether this commit changed anything a host's view should react to (no
    /// upserts, no enumerated removals, and no present-set tombstones).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upserted.is_empty() && self.removed.is_empty() && self.tombstoned == 0
    }
}

/// A sink the streaming sync notifies after each committed chunk.
///
/// Implementations must be cheap and non-blocking (e.g. record into a shared
/// snapshot, push onto a channel); the sync awaits nothing on them. The blanket impl
/// over `Fn(&SyncCommit)` lets a caller pass a closure directly, and composes: a host
/// closure can update its list *and* fold into an
/// [`AccountProgress`](crate::AccountProgress) in one place.
pub trait SyncObserver: Send + Sync {
    /// Receives one durable commit for a scope.
    fn committed(&self, commit: &SyncCommit<'_>);
}

impl<F: Fn(&SyncCommit<'_>) + Send + Sync> SyncObserver for F {
    fn committed(&self, commit: &SyncCommit<'_>) {
        self(commit);
    }
}

/// A [`SyncObserver`] that ignores every commit — for a sync whose caller does not
/// need progress or change events (e.g. a background reconcile).
#[derive(Debug, Clone, Copy, Default)]
pub struct IgnoreCommits;

impl SyncObserver for IgnoreCommits {
    fn committed(&self, _commit: &SyncCommit<'_>) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use engine_core::{
        ids::{MailboxId, MessageId},
        mail::Message,
        membership::Memberships,
        sync::{JmapDataType, SyncScope},
    };

    use super::*;

    fn account() -> engine_core::ids::AccountId {
        engine_core::ids::AccountId::try_from("acct-1").unwrap()
    }

    fn message(id: &str) -> Message {
        Message::new(
            MessageId::try_from(id).unwrap(),
            Memberships::of_one(MailboxId::try_from("a").unwrap()),
        )
    }

    #[test]
    fn closure_observer_receives_upserts_and_removals() {
        let scope = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        };
        let seen: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());
        let observer = |c: &SyncCommit<'_>| {
            seen.lock()
                .unwrap()
                .push((c.fetched, c.upserted.len(), c.removed.len()));
        };
        let upserted = vec![message("m1"), message("m2")];
        let removed = vec![ProviderKey::new("gone").unwrap()];
        observer.committed(&SyncCommit {
            scope: &scope,
            fetched: 2,
            total: Some(5),
            upserted: &upserted,
            removed: &removed,
            tombstoned: 0,
        });
        assert_eq!(*seen.lock().unwrap(), vec![(2, 2, 1)]);
    }

    #[test]
    fn empty_commit_is_flagged() {
        let scope = SyncScope::JmapType {
            account: account(),
            data_type: JmapDataType::Email,
        };
        let commit = SyncCommit {
            scope: &scope,
            fetched: 0,
            total: None,
            upserted: &[],
            removed: &[],
            tombstoned: 0,
        };
        assert!(commit.is_empty());
        IgnoreCommits.committed(&commit);
    }
}
