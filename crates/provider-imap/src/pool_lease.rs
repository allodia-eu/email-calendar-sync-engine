//! The two guards a caller holds on the account's connection budget: a worker's borrowed
//! connection, and a watch's lease on one. Both give their slot back when dropped, because a
//! task can be aborted rather than stopped, and an explicit release would then simply not run.
//!
//! Split from [`crate::pool`], which decides who gets a connection; this is only what giving
//! one back does.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;

use crate::{pool::ImapPool, transport::Connection};

/// Releases a watch's slot in the account's budget. Object-safe so a [`WatchLease`] can hold it
/// without being generic over the stream type — a lease is handed to `ImapWatcher`, and making
/// it generic would spread `S` across the watcher's whole public surface for no benefit.
pub(crate) trait ReleaseWatch: Send + Sync {
    fn release_watch(&self);
}

impl<S: Send + Sync + 'static> ReleaseWatch for ImapPool<S> {
    fn release_watch(&self) {
        self.release_watch_slot();
    }
}

/// A watch's claim on one connection in the account's budget, released when dropped.
///
/// Held by the watcher beside its connection. Dropping it — whether the watch stopped
/// gracefully or its task was aborted — returns the slot, which is why it is a guard rather
/// than a pair of `reserve`/`release` calls a caller can forget to balance.
pub(crate) struct WatchLease {
    watches: Arc<dyn ReleaseWatch>,
    permit: Option<OwnedSemaphorePermit>,
}

impl WatchLease {
    /// A lease on the slot `watches` counted, standing on `permit`.
    pub(crate) fn new(watches: Arc<dyn ReleaseWatch>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            watches,
            permit: Some(permit),
        }
    }
}

impl core::fmt::Debug for WatchLease {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WatchLease").finish_non_exhaustive()
    }
}

impl Drop for WatchLease {
    fn drop(&mut self) {
        if self.permit.is_some() {
            self.watches.release_watch();
        }
    }
}

/// A worker connection borrowed from the pool, parked again when dropped.
///
/// Derefs to the connection, so a caller that used to lock a mutex changes only how it obtains
/// the guard.
pub(crate) struct PooledConnection<S> {
    /// `Some` until dropped; `Option` only so `Drop` can move the connection back into the pool.
    connection: Option<Connection<S>>,
    generation: u64,
    pool: Arc<ImapPool<S>>,
    /// Held for the guard's life; returning it is what lets the next caller in.
    _permit: OwnedSemaphorePermit,
}

impl<S> PooledConnection<S> {
    /// A borrow of `connection`, dialled in `generation`, standing on `permit`. The pool has
    /// already counted it as a worker, and dropping the guard uncounts it.
    pub(crate) fn new(
        connection: Connection<S>,
        generation: u64,
        pool: Arc<ImapPool<S>>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            connection: Some(connection),
            generation,
            pool,
            _permit: permit,
        }
    }

    /// Abandons this connection instead of parking it — for a caller that has left the session
    /// in a state the next borrower must not inherit.
    pub(crate) fn discard(mut self) {
        self.connection = None;
    }

    /// Ends the borrow on the outcome of the work done with it: a success parks the connection,
    /// a failure discards it.
    ///
    /// A failure does not say *which* kind it was — a server `NO` leaves a perfectly good
    /// session, a dropped socket or a desynchronised response does not — and telling them apart
    /// at every call site is more fragile than paying one dial after the rare clean refusal.
    /// What must never happen is the next caller inheriting a dead socket that has not rested
    /// long enough to be checked.
    pub(crate) fn settle<T, E>(self, result: Result<T, E>) -> Result<T, E> {
        if result.is_err() {
            self.discard();
        }
        result
    }
}

impl<S> core::ops::Deref for PooledConnection<S> {
    type Target = Connection<S>;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("a pooled connection is only taken in Drop")
    }
}

impl<S> core::ops::DerefMut for PooledConnection<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("a pooled connection is only taken in Drop")
    }
}

impl<S> Drop for PooledConnection<S> {
    fn drop(&mut self) {
        self.pool.give_back(self.connection.take(), self.generation);
    }
}
