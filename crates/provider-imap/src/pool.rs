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
//! IMAP gives a client no way to discover the server's limit, and only some servers say when it
//! was hit. RFC 5530 has no response code for "too many connections". Yahoo answers the excess
//! `LOGIN` with `NO [LIMIT]`, which the transport reads as `RateLimited` — a refusal to wait out,
//! but still a refusal. Dovecot's `mail_max_userip_connections` — default **10** — rejects at the
//! login stage, where its own documentation notes the client cannot tell what went wrong, and
//! `AUTHENTICATIONFAILED` is specified as a failure *"on which the server is unwilling to
//! elaborate"*: there, exceeding the limit surfaces as a wrong password, and a client that trusts
//! that tells the user their sign-in expired. Either way the account fails to connect, so the
//! only defence is to not need the limit.
//!
//! # The model
//!
//! One budget per account, spent two ways:
//!
//! * **Workers** are fungible. A caller acquires one, the pool hands back a connection that is
//!   already authenticated, and parks it again on drop. A caller that names a mailbox
//!   ([`ImapPool::acquire_for`]) is handed a parked connection that already has it open when there
//!   is one: a body read there skips its `EXAMINE` (`crate::transport_select`), which is half the
//!   round trips of a warm that reads a folder's bodies one after another. Every other caller
//!   `SELECT`s anyway, because the `SELECT` response *is* the data it needs (`UIDVALIDITY`,
//!   `UIDNEXT`, `HIGHESTMODSEQ`), so a preference would save it nothing. A pass that can share its
//!   work borrows extra workers with `try_acquire_for`, which lends only a free permit and never
//!   waits, so that sharing cannot keep another caller waiting.
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
//! **The budget counts sockets, not borrows.** A parked connection holds no permit, so the pool
//! dials only when nothing is parked: a permit is then the only thing standing for a socket, and
//! `parked + borrowed + watches` never exceeds the ceiling. A watch reuses a parked connection
//! for the same reason — dialling one beside a full shelf is how five becomes nine.
//!
//! Reuse is validated once a connection has rested: a parked connection is checked with `NOOP`
//! before it is handed out if it has sat for [`VALIDATE_AFTER_REST`] or longer, because a socket
//! that has sat through a laptop suspend or a phone's radio change looks open and is not. A
//! failed check discards that connection and tries the next. One handed back moments ago is not
//! checked — a sync pass makes many calls in quick succession, and a `NOOP` before each would
//! double their round trips to learn what the previous call just proved. The two ways a young
//! connection can still be dead are covered elsewhere: a call that fails does not park its
//! connection ([`PooledConnection::settle`]), and a host that sees the network change drops every
//! resting connection at once ([`ImapPool::invalidate`]).
//!
//! # A dial refused beside connections that work
//!
//! Five is our number, not the server's, and the server's is shared with every client the user
//! runs, so a dial can fail while the account holds connections that work. While another worker
//! holds one, a caller whose dial failed waits for it to come back instead of failing, and fails
//! only once no worker is left to wait for. When the server *answered* with a refusal (a `LOGIN`
//! `NO`, which is how Dovecot and Gmail say it, `NO [LIMIT]`, or a `BYE` at the greeting), the
//! ceiling also drops by that connection, so the pool stops asking for a session it was just
//! refused: a `NO` to a credential other sessions are logged in with right now is no verdict on
//! the credential. A refused token reaches the pool only after the dial renewed it once
//! (`crate::dial`), so an expired token never lowers the ceiling. [`ImapPool::invalidate`] restores
//! it, since a new network is a new address and Dovecot counts per address. A dial that failed
//! without an answer lowers nothing.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use engine_provider::{ProviderError, ProviderResult};
use futures_util::future::BoxFuture;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
};

pub(crate) use crate::pool_lease::{PooledConnection, WatchLease};
use crate::{error::ImapError, pool_lease::ReleaseWatch, transport::Connection};

/// The most connections one account may hold open at once, across workers and watches.
///
/// Five, matching the only mainstream client that states a number: Thunderbird's desktop
/// `max_cached_connections` default. It sits comfortably under Dovecot's default of 10 while
/// leaving room for an account whose host watches several folders — and unlike that 10 it is a
/// number we control, so it holds on a server whose limit we cannot read.
pub(crate) const DEFAULT_MAX_CONNECTIONS: usize = 5;

/// Connections the pool always keeps available to workers, whatever the host reserves for
/// watches. One is enough to make progress; zero is a deadlock.
const MIN_WORKER_CONNECTIONS: usize = 1;

/// How long a connection may rest before it is proved alive with `NOOP` on its next hand-out.
///
/// Long enough that the calls of one sync pass — which follow each other within milliseconds —
/// reuse a connection without a check, and short enough that the gap *between* passes, where
/// sockets actually die, always pays one. It is measured on the wall clock, not a monotonic one:
/// Linux's monotonic clock stops while the machine is suspended, so a laptop that slept for an
/// hour would otherwise look like it rested for a second.
pub(crate) const VALIDATE_AFTER_REST: Duration = Duration::from_secs(30);

/// Dials, authenticates and negotiates one fresh connection.
///
/// A factory rather than a config, so the pool never needs to know how a connection is made:
/// the live path hands it the TCP + TLS + `LOGIN` sequence, and a test hands it a scripted
/// in-memory stream. Without this seam the pool would only be testable against a real server,
/// which is the same trap the pool exists to fix.
pub(crate) type Dial<S> =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Connection<S>, ImapError>> + Send + Sync>;

/// A connection resting in the pool.
struct Parked<S> {
    connection: Connection<S>,
    /// The generation it was dialled in; a connection from a superseded generation is closed
    /// rather than reused (see [`ImapPool::invalidate`]).
    generation: u64,
    /// When it was parked, to decide whether it has rested long enough to need a `NOOP`.
    rested_at: SystemTime,
}

/// A bounded pool of authenticated IMAP connections for one account.
pub(crate) struct ImapPool<S> {
    dial: Dial<S>,
    /// One permit per connection the account may hold — workers and watches alike. Its total is
    /// always [`ceiling`](Self::ceiling).
    permits: Arc<Semaphore>,
    /// Authenticated connections not currently in use. A plain `std::sync::Mutex` because it is
    /// only ever held to push or pop, never across an `await` — which is what lets a dropped
    /// guard park its connection without needing an async destructor.
    parked: Mutex<VecDeque<Parked<S>>>,
    /// How many permits are currently held by watches, so [`ImapPool::reserve_watch`] can
    /// refuse to starve the workers.
    watches: Mutex<usize>,
    max_connections: usize,
    /// The ceiling in force: `max_connections`, less one for each session the server refused
    /// beside connections this account already held (see the module docs).
    ceiling: AtomicUsize,
    /// Worker connections currently borrowed, so a caller whose dial failed knows whether one
    /// is coming back.
    workers: AtomicUsize,
    /// Signalled whenever a worker's borrow ends, parked or discarded, for the callers waiting
    /// on one after a failed dial.
    returned: Notify,
    /// See [`VALIDATE_AFTER_REST`]; a parameter so the tests can make every reuse a checked one.
    validate_after: Duration,
    generation: AtomicU64,
}

impl<S> core::fmt::Debug for ImapPool<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImapPool")
            .field("max_connections", &self.max_connections)
            .field("ceiling", &self.ceiling())
            .field("available", &self.permits.available_permits())
            .field("parked", &self.parked.lock().map_or(0, |p| p.len()))
            .finish_non_exhaustive()
    }
}

impl<S> ImapPool<S> {
    /// A pool of at most `max_connections`, dialling through `dial`, that proves a connection
    /// alive before reusing it once it has rested for `validate_after`.
    ///
    /// `max_connections` is clamped to at least [`MIN_WORKER_CONNECTIONS`]: a pool that can hold
    /// no connections cannot sync, and silently accepting zero would turn a configuration slip
    /// into an account that never loads.
    pub(crate) fn new(
        dial: Dial<S>,
        max_connections: usize,
        validate_after: Duration,
    ) -> Arc<Self> {
        let max_connections = max_connections.max(MIN_WORKER_CONNECTIONS);
        Arc::new(Self {
            dial,
            permits: Arc::new(Semaphore::new(max_connections)),
            parked: Mutex::new(VecDeque::new()),
            watches: Mutex::new(0),
            max_connections,
            ceiling: AtomicUsize::new(max_connections),
            workers: AtomicUsize::new(0),
            returned: Notify::new(),
            validate_after,
            generation: AtomicU64::new(0),
        })
    }

    /// The ceiling in force now.
    pub(crate) fn ceiling(&self) -> usize {
        self.ceiling.load(Ordering::SeqCst)
    }

    /// Parks a connection that was dialled outside the pool, so the next caller reuses it — the
    /// one an account's connect opened to prove the credentials and read the capabilities.
    pub(crate) fn adopt(&self, connection: Connection<S>) {
        self.park(connection, self.generation.load(Ordering::SeqCst));
    }

    fn reserved_watches(&self) -> usize {
        *self.watches.lock().expect("pool watch counter poisoned")
    }

    /// How many more watches may be reserved before the workers' floor is reached. The host
    /// reads this to warn *before* offering the user a choice that cannot be honoured.
    pub(crate) fn watch_headroom(&self) -> usize {
        self.ceiling()
            .saturating_sub(MIN_WORKER_CONNECTIONS)
            .saturating_sub(self.reserved_watches())
    }

    /// How many connections workers can hold at once right now: the ceiling less the watches.
    /// It moves as watches come and go, and when a refusal lowers the ceiling.
    pub(crate) fn worker_capacity(&self) -> usize {
        self.ceiling()
            .saturating_sub(self.reserved_watches())
            .max(MIN_WORKER_CONNECTIONS)
    }

    /// Marks every connection dialled so far as superseded, so none is reused, drops the ones
    /// currently resting, and restores a ceiling a refusal lowered. In-flight guards finish
    /// normally and are discarded on drop.
    ///
    /// This is the answer to a network that went away: the sockets look open and are not, and
    /// `NOOP`-ing each one costs a timeout apiece. The ceiling comes back with it because the
    /// device now dials from a different address, and a server's session limit is commonly
    /// counted per address.
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.parked
            .lock()
            .expect("pool parked-connection mutex poisoned")
            .clear();
        let lowered = self
            .max_connections
            .saturating_sub(self.ceiling.swap(self.max_connections, Ordering::SeqCst));
        self.permits.add_permits(lowered);
    }

    /// Pops a parked connection: one with `mailbox` open when the caller named one and there is
    /// one, else the most recently parked, which is the likeliest to be alive and to need no
    /// check. Connections from a superseded generation are dropped along the way.
    fn take_parked(&self, generation: u64, mailbox: Option<&str>) -> Option<Parked<S>> {
        let mut parked = self
            .parked
            .lock()
            .expect("pool parked-connection mutex poisoned");
        parked.retain(|candidate| candidate.generation >= generation);
        let open = mailbox.and_then(|mailbox| {
            parked
                .iter()
                .rposition(|candidate| candidate.connection.open_validity(mailbox).is_some())
        });
        match open {
            Some(at) => parked.remove(at),
            None => parked.pop_back(),
        }
    }

    /// Whether a parked connection must answer `NOOP` before it is handed out. A clock that
    /// moved backwards cannot prove a short rest, so it counts as a long one.
    fn needs_check(&self, parked: &Parked<S>, always: bool) -> bool {
        always
            || parked
                .rested_at
                .elapsed()
                .map_or(true, |rested| rested >= self.validate_after)
    }

    /// Returns a connection to the pool, or drops it if the generation has moved on.
    fn park(&self, connection: Connection<S>, generation: u64) {
        if generation < self.generation.load(Ordering::SeqCst) {
            return;
        }
        self.parked
            .lock()
            .expect("pool parked-connection mutex poisoned")
            .push_back(Parked {
                connection,
                generation,
                rested_at: SystemTime::now(),
            });
    }

    /// Ends a worker's borrow: parks its connection when it has one to give back, and wakes
    /// the callers waiting on a connection after a failed dial.
    pub(crate) fn give_back(&self, connection: Option<Connection<S>>, generation: u64) {
        if let Some(connection) = connection {
            self.park(connection, generation);
        }
        self.workers.fetch_sub(1, Ordering::SeqCst);
        self.returned.notify_waiters();
    }

    /// Returns a watch's slot to the workers.
    pub(crate) fn release_watch_slot(&self) {
        let mut watches = self.watches.lock().expect("pool watch counter poisoned");
        *watches = watches.saturating_sub(1);
    }

    /// Gives up the connection `permit` stood for, for as long as the ceiling stays lowered.
    fn lower_ceiling(&self, permit: OwnedSemaphorePermit) {
        permit.forget();
        self.ceiling.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a failed dial was the server declining a session, rather than a path that failed.
/// Only an answer lowers the ceiling: a network that is down says nothing about the server.
fn refuses_sessions(err: &ImapError) -> bool {
    matches!(
        err,
        ImapError::Auth(_) | ImapError::RateLimited(_) | ImapError::Bye(_)
    )
}

/// The two operations that actually touch a connection, so they carry the stream bound the
/// transport needs. Everything above is bookkeeping and stays bound-free — which is what lets
/// `PooledConnection`'s `Drop` park its connection, since a `Drop` impl cannot add bounds.
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ImapPool<S> {
    /// Takes a worker connection: a parked one if any is usable, else a fresh dial.
    ///
    /// Waits when every permit is spent, rather than dialling past the budget — the whole
    /// purpose being that the account's socket count is bounded by this number and not by how
    /// many folders a host happens to sync.
    ///
    /// # Errors
    ///
    /// [`ImapError`] if a fresh connection has to be dialled, the dial fails, and no other
    /// worker holds a connection to wait for.
    pub(crate) async fn acquire(self: &Arc<Self>) -> Result<PooledConnection<S>, ImapError> {
        self.acquire_worker(None, false).await
    }

    /// [`acquire`](Self::acquire), preferring a parked connection that already has `mailbox`
    /// open — for a read that can then skip opening it.
    ///
    /// # Errors
    ///
    /// As [`acquire`](Self::acquire).
    pub(crate) async fn acquire_for(
        self: &Arc<Self>,
        mailbox: &str,
    ) -> Result<PooledConnection<S>, ImapError> {
        self.acquire_worker(Some(mailbox), false).await
    }

    /// [`acquire`](Self::acquire), but proving any parked connection alive however briefly it
    /// rested — for a retry, whose first attempt has just shown that a young connection can be
    /// a dead one.
    ///
    /// # Errors
    ///
    /// As [`acquire`](Self::acquire).
    pub(crate) async fn acquire_checked(
        self: &Arc<Self>,
    ) -> Result<PooledConnection<S>, ImapError> {
        self.acquire_worker(None, true).await
    }

    async fn acquire_worker(
        self: &Arc<Self>,
        mailbox: Option<&str>,
        always_check: bool,
    ) -> Result<PooledConnection<S>, ImapError> {
        loop {
            let permit = Arc::clone(&self.permits)
                .acquire_owned()
                .await
                .expect("pool semaphore is never closed");
            // Armed before the dial, so a connection handed back while the dial is failing still
            // wakes this caller.
            let mut returned = std::pin::pin!(self.returned.notified());
            returned.as_mut().enable();
            let err = match self.take_or_dial(mailbox, always_check).await {
                Ok((connection, generation)) => {
                    self.workers.fetch_add(1, Ordering::SeqCst);
                    return Ok(PooledConnection::new(
                        connection,
                        generation,
                        Arc::clone(self),
                        permit,
                    ));
                }
                Err(err) => err,
            };
            if self.workers.load(Ordering::SeqCst) == 0 {
                return Err(err);
            }
            if refuses_sessions(&err) {
                self.lower_ceiling(permit);
            } else {
                drop(permit);
            }
            returned.await;
        }
    }

    /// Hands out a usable parked connection, or dials one when none is left — never both, so a
    /// dial only happens with nothing on the shelf and the permit its caller holds is the only
    /// thing standing for the new socket.
    async fn take_or_dial(
        &self,
        mailbox: Option<&str>,
        always_check: bool,
    ) -> Result<(Connection<S>, u64), ImapError> {
        let generation = self.generation.load(Ordering::SeqCst);
        // A failed check is not an error — it is a connection that died while resting, which is
        // the normal fate of a socket on a device that sleeps.
        while let Some(mut parked) = self.take_parked(generation, mailbox) {
            if !self.needs_check(&parked, always_check) || is_alive(&mut parked.connection).await {
                return Ok((parked.connection, parked.generation));
            }
        }
        Ok(((self.dial)().await?, generation))
    }

    /// Takes a connection out of the pool for a watch to own, spending one permit for its whole
    /// lifetime.
    ///
    /// A parked connection is reused when there is one: the watcher's `EXAMINE` reads whatever
    /// untagged backlog it carries before the first `IDLE`, so it starts as clean as a fresh
    /// dial, and dialling beside a parked connection would put one socket over the budget.
    ///
    /// # Errors
    ///
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState) when the
    /// reservation would leave the workers below their floor — the host must watch fewer
    /// folders, or poll. The classified dial failure otherwise.
    pub(crate) async fn reserve_watch(
        self: &Arc<Self>,
    ) -> ProviderResult<(Connection<S>, WatchLease)> {
        {
            let mut watches = self.watches.lock().expect("pool watch counter poisoned");
            let ceiling = self.ceiling();
            // Refuse the reservation that would take the last worker: with a budget of 5 this
            // allows 4 watches and always keeps 1 free to sync. The host's own limit on watched
            // folders is normally stricter — this is the floor that makes over-reserving
            // impossible, not the policy that makes it sensible.
            if *watches + MIN_WORKER_CONNECTIONS >= ceiling {
                return Err(ProviderError::invalid_state(format!(
                    "cannot watch another folder: {} of this account's {ceiling} connections are \
                     already reserved for push, and at least {MIN_WORKER_CONNECTIONS} must stay \
                     free to sync",
                    *watches,
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
        let lease = WatchLease::new(Arc::clone(self) as Arc<dyn ReleaseWatch>, permit);
        let (connection, _generation) = self.take_or_dial(None, false).await?;
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

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pool_refusal_tests.rs"]
mod refusal_tests;

#[path = "pool_spare.rs"]
mod spare;
