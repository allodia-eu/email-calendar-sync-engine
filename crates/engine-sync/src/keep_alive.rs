//! Keeping a scope lease while a long pass waits on its provider.

use core::{future::Future, time::Duration};

use engine_store::{Store, SyncLease};
use tokio::time::Instant;

/// Renews a scope lease every third of its TTL for as long as its pass keeps waiting.
///
/// The deadline runs across waits, so a pass of many quick chunks renews as surely as one slow
/// chunk does: a backfill paced under a server's quota can run for hours a page at a time, and a
/// lease it never renewed would lapse into the next poll's claim.
pub(crate) struct KeepAlive<'a, S> {
    store: &'a S,
    lease: &'a SyncLease,
    ttl: Duration,
    due: Instant,
}

impl<'a, S: Store> KeepAlive<'a, S> {
    pub(crate) fn new(store: &'a S, lease: &'a SyncLease, ttl: Duration) -> Self {
        Self {
            store,
            lease,
            ttl,
            due: Instant::now() + ttl / 3,
        }
    }

    /// Runs `work`, renewing the lease whenever it falls due meanwhile.
    ///
    /// A renewal refused as stale means another worker has the scope (the process was suspended
    /// past the lease, say); the pass's next commit is refused and the pass claims again, so there
    /// is nothing to do here. A renewal that fails for any other reason is tried again at the
    /// next tick, still well inside the lease.
    pub(crate) async fn around<F: Future>(&mut self, work: F) -> F::Output {
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                done = &mut work => return done,
                () = tokio::time::sleep_until(self.due) => {
                    let _ = self.store.renew_sync_scope_lease(self.lease, self.ttl).await;
                    self.due = Instant::now() + self.ttl / 3;
                }
            }
        }
    }
}
