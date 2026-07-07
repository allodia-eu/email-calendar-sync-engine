//! Account-level sync progress aggregation.

use std::{collections::HashMap, sync::Mutex};

use engine_core::sync::SyncScope;

use crate::{SyncCommit, SyncObserver};

/// A point-in-time account-level progress reading: "downloaded `fetched` of `total`".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressSnapshot {
    /// Whether any pass is currently in flight for the account.
    pub active: bool,
    /// Objects committed so far across every reporting scope.
    pub fetched: usize,
    /// The account total, or `None` while still indeterminate (see
    /// [`AccountProgress`]).
    pub total: Option<usize>,
}

/// Aggregates per-scope streaming commits into one account-level figure, so a host
/// renders a single "downloaded Y of X" bar over a **concurrent per-folder
/// fan-out** — the engine owning what a host would otherwise reimplement.
///
/// Two hard-won behaviors it bakes in:
///
/// - **The total stays indeterminate** (`None`) until *every* expected scope has reported a commit
///   carrying a `total`, so the bar never jumps "12 of 12 → 83 of 83" as later folders begin
///   streaming.
/// - **In-flight passes are counted** ([`begin`](Self::begin)/[`finish`](Self::finish)), so a
///   concurrent on-demand sync of one folder cannot blank the bar while a background pass over the
///   others is still running.
///
/// It is a [`SyncObserver`], so a host composes it with its own list-updating
/// closure: `move |c| { my_view.apply(c); progress.committed(c); }`. Thread-safe
/// (an internal lock), so every concurrent folder stream reports into one instance.
#[derive(Debug)]
pub struct AccountProgress {
    inner: Mutex<Aggregate>,
}

#[derive(Debug)]
struct Aggregate {
    expected: usize,
    active_passes: usize,
    per_scope: HashMap<SyncScope, ScopeProgress>,
}

#[derive(Debug, Clone, Copy)]
struct ScopeProgress {
    fetched: usize,
    total: Option<usize>,
}

impl AccountProgress {
    /// A fresh aggregator expecting `expected_scopes` scopes to report this run
    /// (e.g. the number of folders a host is about to stream concurrently).
    #[must_use]
    pub fn new(expected_scopes: usize) -> Self {
        Self {
            inner: Mutex::new(Aggregate {
                expected: expected_scopes,
                active_passes: 0,
                per_scope: HashMap::new(),
            }),
        }
    }

    /// Marks a pass as started — a host calls this as each folder stream begins, so
    /// the snapshot reads `active` while any pass runs.
    pub fn begin(&self) {
        self.lock().active_passes += 1;
    }

    /// Marks a pass as finished — a host calls this as each folder stream ends.
    pub fn finish(&self) {
        let mut agg = self.lock();
        agg.active_passes = agg.active_passes.saturating_sub(1);
    }

    /// The current account-level progress.
    #[must_use]
    pub fn snapshot(&self) -> ProgressSnapshot {
        let agg = self.lock();
        let fetched = agg.per_scope.values().map(|s| s.fetched).sum();
        // The total is known only once every expected scope has reported one — so a
        // late-starting folder never rebases an already-shown denominator upward.
        let with_total = agg.per_scope.values().filter(|s| s.total.is_some()).count();
        let total = if with_total >= agg.expected && agg.expected > 0 {
            Some(agg.per_scope.values().filter_map(|s| s.total).sum())
        } else {
            None
        };
        ProgressSnapshot {
            active: agg.active_passes > 0,
            fetched,
            total,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Aggregate> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SyncObserver for AccountProgress {
    fn committed(&self, commit: &SyncCommit<'_>) {
        let mut agg = self.lock();
        agg.per_scope.insert(
            commit.scope.clone(),
            ScopeProgress {
                fetched: commit.fetched,
                total: commit.total,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use engine_core::{
        ids::{AccountId, MailboxId},
        sync::SyncScope,
    };

    use super::*;

    fn account() -> AccountId {
        AccountId::try_from("acct-1").unwrap()
    }

    fn folder(name: &str) -> SyncScope {
        SyncScope::ImapMailbox {
            account: account(),
            mailbox: MailboxId::try_from(name).unwrap(),
        }
    }

    fn commit_to(
        progress: &AccountProgress,
        scope: &SyncScope,
        fetched: usize,
        total: Option<usize>,
    ) {
        progress.committed(&SyncCommit {
            scope,
            fetched,
            total,
            upserted: &[],
            removed: &[],
            tombstoned: 0,
        });
    }

    #[test]
    fn total_stays_indeterminate_until_every_folder_reports() {
        let progress = AccountProgress::new(2);
        let inbox = folder("INBOX");
        let sent = folder("Sent");

        // Only the first folder has reported: fetched counts, but the total must not
        // show yet (or the bar would read "3 of 10" then jump when Sent reports).
        commit_to(&progress, &inbox, 3, Some(10));
        let snap = progress.snapshot();
        assert_eq!(snap.fetched, 3);
        assert_eq!(snap.total, None);

        // Both folders have now reported a total → the denominator is complete.
        commit_to(&progress, &sent, 2, Some(4));
        let snap = progress.snapshot();
        assert_eq!(snap.fetched, 5);
        assert_eq!(snap.total, Some(14));

        // A later commit on INBOX updates its running count, not the denominator.
        commit_to(&progress, &inbox, 10, Some(10));
        let snap = progress.snapshot();
        assert_eq!(snap.fetched, 12);
        assert_eq!(snap.total, Some(14));
    }

    #[test]
    fn active_tracks_in_flight_passes() {
        let progress = AccountProgress::new(1);
        assert!(!progress.snapshot().active);
        progress.begin();
        progress.begin();
        assert!(progress.snapshot().active);
        progress.finish();
        assert!(progress.snapshot().active); // one pass still running
        progress.finish();
        assert!(!progress.snapshot().active);
        progress.finish(); // saturating: never underflows
        assert!(!progress.snapshot().active);
    }
}
