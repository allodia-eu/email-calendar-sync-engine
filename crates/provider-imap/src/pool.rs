//! A bounded, reusable pool of authenticated IMAP connections for one account.
//!
//! # Why an adapter needs this at all
//!
//! IMAP allows one selected mailbox per connection, so a client that syncs folders
//! concurrently needs one connection per folder *in flight*. The trap is the difference
//! between "in flight" and "held": an adapter bound to a mailbox for its whole lifetime holds
//! a socket for its whole lifetime, and a host that builds one adapter per folder is then
//! holding one socket per folder whether it is syncing or idle. Five folders on two accounts
//! is ten standing sockets, re-dialled in one burst every time a mobile network drops — and
//! nothing in the count is visible from the host, which only ever asked for a folder.
//!
//! IMAP gives a client no way to discover the server's limit and no way to be told it was hit.
//! RFC 5530 has no response code for "too many connections"; `[LIMIT]` is about limits on an
//! operation, and `AUTHENTICATIONFAILED` is specified as a failure *"on which the server is
//! unwilling to elaborate"*. Dovecot's `mail_max_userip_connections` — default **10** — rejects
//! at the login stage, where its own documentation notes the client cannot tell what went
//! wrong. So exceeding the limit does not surface as throttling; it surfaces as a wrong
//! password, and a client that trusts that tells the user their sign-in expired. The only
//! defence is to not need the limit.
//!
//! # The model
//!
//! One budget per account, spent two ways:
//!
//! * **Workers** are fungible. A caller acquires one, the pool hands back a connection that is
//!   already authenticated — preferring one that already has the wanted mailbox selected, since
//!   `SELECT` costs a round trip and discards the CONDSTORE context — and parks it again on drop.
//! * **Watches** are not. A connection in `IDLE` is blocked mid-command: to reuse it you must send
//!   `DONE`, await the tagged completion, work, and re-`IDLE`, which means a window where the
//!   mailbox is not being watched at all. So a watch takes a connection *out* of the pool for its
//!   lifetime and the pool never hands it to anyone else. It still spends from the same budget,
//!   which is the point — the two kinds cannot each be under their own limit and together over the
//!   server's.
//!
//! [`ImapPool::reserve_watch`] refuses a reservation that would leave no worker, because a
//! budget spent entirely on watches is an account that can never sync — a deadlock rather than
//! a degradation. The refusal is the mechanism; deciding what to *tell* the user about it is a
//! product question and belongs to the host.
//!
//! Reuse is validated, never assumed: a parked connection is checked with `NOOP` before it is
//! handed out, because a socket that has sat through a laptop suspend or a phone's radio change
//! looks open and is not. A failed check discards that connection and tries the next.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{OwnedSemaphorePermit, Semaphore},
};

use crate::{error::ImapError, transport::Connection};

/// The most connections one account may hold open at once, across workers and watches.
///
/// Five, matching the only mainstream client that states a number: Thunderbird's desktop
/// `max_cached_connections` default. It sits comfortably under Dovecot's default of 10 while
/// leaving room for an account whose host watches several folders — and unlike that 10 it is a
/// number we control, so it holds on a server whose limit we cannot read.
pub const DEFAULT_MAX_CONNECTIONS: usize = 5;

/// Connections the pool always keeps available to workers, whatever the host reserves for
/// watches. One is enough to make progress; zero is a deadlock.
const MIN_WORKER_CONNECTIONS: usize = 1;

/// Dials, authenticates and negotiates one fresh connection.
///
/// A factory rather than a config, so the pool never needs to know how a connection is made:
/// the live path hands it the TCP + TLS + `LOGIN` sequence, and a test hands it a scripted
/// in-memory stream. Without this seam the pool would only be testable against a real server,
/// which is the same trap the pool exists to fix.
pub(crate) type Dial<S> =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Connection<S>, ImapError>> + Send + Sync>;

/// A connection resting in the pool, with the mailbox it last selected.
struct Parked<S> {
    connection: Connection<S>,
    /// The mailbox this connection has selected, so a later caller wanting the same one can
    /// skip the `SELECT`. `None` for a connection that has selected nothing yet.
    selected: Option<String>,
    /// The generation it was dialled in; a connection from a superseded generation is closed
    /// rather than reused (see [`ImapPool::invalidate`]).
    generation: u64,
}

/// A bounded pool of authenticated IMAP connections for one account.
pub struct ImapPool<S> {
    dial: Dial<S>,
    /// One permit per connection the account may hold — workers and watches alike.
    permits: Arc<Semaphore>,
    /// Authenticated connections not currently in use. A plain `std::sync::Mutex` because it is
    /// only ever held to push or pop, never across an `await` — which is what lets a dropped
    /// guard park its connection without needing an async destructor.
    parked: Mutex<VecDeque<Parked<S>>>,
    /// How many permits are currently held by watches, so [`ImapPool::reserve_watch`] can
    /// refuse to starve the workers.
    watches: Mutex<usize>,
    max_connections: usize,
    generation: AtomicU64,
}

impl<S> core::fmt::Debug for ImapPool<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapPool")
            .field("max_connections", &self.max_connections)
            .field("available", &self.permits.available_permits())
            .field("parked", &self.parked.lock().map_or(0, |p| p.len()))
            .finish_non_exhaustive()
    }
}

impl<S> ImapPool<S> {
    /// A pool of at most `max_connections`, dialling through `dial`.
    ///
    /// `max_connections` is clamped to at least [`MIN_WORKER_CONNECTIONS`]: a pool that can hold
    /// no connections cannot sync, and silently accepting zero would turn a configuration slip
    /// into an account that never loads.
    pub(crate) fn new(dial: Dial<S>, max_connections: usize) -> Arc<Self> {
        let max_connections = max_connections.max(MIN_WORKER_CONNECTIONS);
        Arc::new(Self {
            dial,
            permits: Arc::new(Semaphore::new(max_connections)),
            parked: Mutex::new(VecDeque::new()),
            watches: Mutex::new(0),
            max_connections,
            generation: AtomicU64::new(0),
        })
    }

    /// The pool's ceiling — every connection this account may hold, workers and watches.
    pub(crate) fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// How many more watches may be reserved before the workers' floor is reached. The host
    /// reads this to warn *before* offering the user a choice that cannot be honoured.
    pub(crate) fn watch_headroom(&self) -> usize {
        let reserved = *self.watches.lock().expect("pool watch counter poisoned");
        self.max_connections
            .saturating_sub(MIN_WORKER_CONNECTIONS)
            .saturating_sub(reserved)
    }

    /// Marks every connection dialled so far as superseded, so none is reused, and drops the
    /// ones currently resting. In-flight guards finish normally and are discarded on drop.
    ///
    /// This is the answer to a network that went away: the sockets look open and are not, and
    /// `NOOP`-ing each one costs a timeout apiece.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.parked
            .lock()
            .expect("pool parked-connection mutex poisoned")
            .clear();
    }

    /// Pops the best parked candidate: one already on `mailbox` if there is one, else any.
    /// Connections from a superseded generation are dropped along the way.
    fn take_parked(&self, mailbox: Option<&str>, generation: u64) -> Option<Parked<S>> {
        let mut parked = self
            .parked
            .lock()
            .expect("pool parked-connection mutex poisoned");
        parked.retain(|candidate| candidate.generation >= generation);
        if let Some(wanted) = mailbox {
            if let Some(index) = parked
                .iter()
                .position(|candidate| candidate.selected.as_deref() == Some(wanted))
            {
                return parked.remove(index);
            }
        }
        parked.pop_front()
    }

    /// Returns a connection to the pool, or drops it if the generation has moved on.
    fn park(&self, connection: Connection<S>, selected: Option<String>, generation: u64) {
        if generation < self.generation.load(Ordering::SeqCst) {
            return;
        }
        self.parked
            .lock()
            .expect("pool parked-connection mutex poisoned")
            .push_back(Parked {
                connection,
                selected,
                generation,
            });
    }
}

/// The two operations that actually touch a connection, so they carry the stream bound the
/// transport needs. Everything above is bookkeeping and stays bound-free — which is what lets
/// `PooledConnection`'s `Drop` park its connection, since a `Drop` impl cannot add bounds.
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapPool<S> {
    /// Takes a worker connection, preferring one that already has `mailbox` selected.
    ///
    /// Waits when every permit is spent, rather than dialling past the budget — the whole
    /// purpose being that the account's socket count is bounded by this number and not by how
    /// many folders a host happens to sync.
    ///
    /// # Errors
    ///
    /// [`ImapError`] if a fresh connection has to be dialled and the dial fails.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        mailbox: Option<&str>,
    ) -> Result<PooledConnection<S>, ImapError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("pool semaphore is never closed");
        let generation = self.generation.load(Ordering::SeqCst);
        // Try parked connections before dialling. Each is validated, and a failed check is not
        // an error — it is a connection that died while resting, which is the normal fate of a
        // socket on a device that sleeps.
        while let Some(mut parked) = self.take_parked(mailbox, generation) {
            if is_alive(&mut parked.connection).await {
                return Ok(PooledConnection {
                    connection: Some(parked.connection),
                    selected: parked.selected,
                    generation: parked.generation,
                    pool: Arc::clone(self),
                    _permit: permit,
                });
            }
        }
        let connection = (self.dial)().await?;
        Ok(PooledConnection {
            connection: Some(connection),
            selected: None,
            generation,
            pool: Arc::clone(self),
            _permit: permit,
        })
    }

    /// Takes a connection out of the pool for a watch to own, spending one permit for its whole
    /// lifetime.
    ///
    /// # Errors
    ///
    /// [`ImapError::bad`] when the reservation would leave the workers below their floor — the
    /// host must reduce how many folders it watches. [`ImapError`] if the dial fails.
    pub(crate) async fn reserve_watch(
        self: &Arc<Self>,
    ) -> Result<(Connection<S>, WatchLease), ImapError> {
        {
            let mut watches = self.watches.lock().expect("pool watch counter poisoned");
            // Refuse the reservation that would take the last worker: with a budget of 5 this
            // allows 4 watches and always keeps 1 free to sync. The host's own limit on watched
            // folders is normally stricter — this is the floor that makes over-reserving
            // impossible, not the policy that makes it sensible.
            if *watches + MIN_WORKER_CONNECTIONS >= self.max_connections {
                return Err(ImapError::bad(format!(
                    "cannot watch another folder: {} of this account's {} connections are \
                     already reserved for push, and at least {MIN_WORKER_CONNECTIONS} must stay \
                     free to sync",
                    *watches, self.max_connections,
                )));
            }
            *watches += 1;
        }
        // From here the slot is ours, so every exit must return it. The lease exists before the
        // dial for exactly that reason: if the dial fails, dropping it releases the slot.
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("pool semaphore is never closed");
        let lease = WatchLease {
            watches: Arc::clone(self) as Arc<dyn ReleaseWatch>,
            permit: Some(permit),
        };
        // A watch always dials fresh. A parked connection may have a mailbox selected and a
        // pending untagged backlog, and an IDLE that begins mid-backlog reports events the
        // watcher's caller has already seen.
        let connection = (self.dial)().await?;
        Ok((connection, lease))
    }
}

/// Whether a resting connection is still usable, proved with the cheapest round trip in the
/// protocol.
///
/// A TCP socket that sat through a laptop suspend, a phone's radio change or a NAT idle-timeout
/// is indistinguishable from a live one until something is written to it. So a pool that trusts
/// an open socket fails on the *caller's* command instead of on a throwaway, and the caller
/// reports it as a server fault. Lives here rather than on the transport because being sure
/// before lending is the pool's problem, not the protocol's.
async fn is_alive<S: AsyncRead + AsyncWrite + Unpin + Send>(
    connection: &mut Connection<S>,
) -> bool {
    connection.command("NOOP").await.is_ok()
}

/// Releases a watch's slot in the account's budget. Object-safe so a [`WatchLease`] can hold it
/// without being generic over the stream type — a lease is handed to `ImapWatcher`, and making
/// it generic would spread `S` across the watcher's whole public surface for no benefit.
trait ReleaseWatch: Send + Sync {
    fn release_watch(&self);
}

impl<S: Send + Sync + 'static> ReleaseWatch for ImapPool<S> {
    fn release_watch(&self) {
        let mut watches = self.watches.lock().expect("pool watch counter poisoned");
        *watches = watches.saturating_sub(1);
    }
}

/// A watch's claim on one connection in the account's budget, released when dropped.
///
/// Held by the watcher beside its connection. Dropping it — whether the watch stopped
/// gracefully or its task was aborted — returns the slot, which is why it is a guard rather
/// than a pair of `reserve`/`release` calls a caller can forget to balance.
pub struct WatchLease {
    watches: Arc<dyn ReleaseWatch>,
    permit: Option<OwnedSemaphorePermit>,
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
/// the guard. It records the mailbox selected through [`PooledConnection::note_selected`] so the
/// next caller wanting that mailbox can skip the `SELECT`.
pub(crate) struct PooledConnection<S> {
    /// `Some` until dropped; `Option` only so `Drop` can move the connection back into the pool.
    connection: Option<Connection<S>>,
    selected: Option<String>,
    generation: u64,
    pool: Arc<ImapPool<S>>,
    /// Held for the guard's life; returning it is what lets the next caller in.
    _permit: OwnedSemaphorePermit,
}

impl<S> PooledConnection<S> {
    /// The mailbox this connection currently has selected, if the pool knows.
    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Records that `mailbox` is now selected on this connection, so the pool can offer it to a
    /// later caller wanting the same one.
    pub(crate) fn note_selected(&mut self, mailbox: impl Into<String>) {
        self.selected = Some(mailbox.into());
    }

    /// Abandons this connection instead of parking it — for a caller that has left the session
    /// in a state the next borrower must not inherit.
    pub(crate) fn discard(mut self) {
        self.connection = None;
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
        if let Some(connection) = self.connection.take() {
            self.pool
                .park(connection, self.selected.take(), self.generation);
        }
    }
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
