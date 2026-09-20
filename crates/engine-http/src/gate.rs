//! The account-wide ceiling on requests in flight.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

/// How many requests **one account** may have in flight at once, shared by every provider
/// that account connects.
///
/// # Why this is not a per-provider number
///
/// A server that limits concurrency counts *requests*, and it counts them against the
/// account. Microsoft says so in the refusal itself — `ApplicationThrottled: Application is
/// over its MailboxConcurrency limit` — and a live 56-folder mailbox draws the line exactly
/// where the documentation puts it: measured over alternating widths inside one run, **4
/// concurrent requests is clean over hundreds of requests and 5 is refused 13% of the time**.
///
/// It does not care which request is which. The same mailbox, given four folder syncs plus
/// one body warm, refuses at the same 5 — and refuses *both kinds*, 41 folder pages and 19
/// source fetches in one measured block. So a bound that sees only one workload cannot hold
/// the total: clamping the folder fan-out to 4 leaves any concurrent body warm to put the
/// account straight back over.
///
/// That is the whole argument for putting it here. [`send_retrying`](crate::send_retrying) is
/// the one place *every* HTTP request from *every* adapter passes through
/// (`docs/agent-guidance/http-throttling.md`), so it is the only place that can count what
/// the server counts.
///
/// # One gate, one account
///
/// A host builds **one gate per account** and hands clones to every provider it connects for
/// that account — its folder-bound mail providers, its calendar, its contacts. Clones share
/// one ceiling; that is the point, and a clone that bounded itself would be four gates of
/// four.
///
/// Getting the *scope* wrong is the failure this type exists to prevent, and it is not
/// hypothetical: a host had already written this bound by hand as a semaphore on its
/// per-account token source, correctly set to 4 — and then built the core twice in one
/// process, which is ordinary on Android where a one-time sync worker and the periodic one
/// overlap. Two cores, two semaphores, eight concurrent requests, and a mailbox refusing 40%
/// of them. The number was right and the thing it was attached to was wrong.
///
/// # Who sets the limit
///
/// The **adapter**, not the host, via [`narrow_to`](Self::narrow_to) when it connects: the
/// ceiling is a fact about the server, and a host that had to know it would be branching on
/// which provider it is talking to, which is the engine leaking
/// (`docs/agent-guidance/providers.md`). A new gate bounds nothing, so a host that wires none
/// gets exactly today's behaviour rather than a silent serialization.
///
/// Narrowing only ever narrows, so an account whose adapters disagree is bound by the
/// strictest of them whatever order they connect in.
///
/// ```
/// use engine_http::RequestGate;
///
/// let gate = RequestGate::new();
/// assert_eq!(gate.limit(), None); // bounds nothing until an adapter states a ceiling
/// gate.narrow_to(4);
/// gate.narrow_to(20); // a looser claim never raises a ceiling already lowered
/// assert_eq!(gate.limit(), Some(4));
/// ```
#[derive(Clone, Default)]
pub struct RequestGate(Arc<Inner>);

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    /// Woken as each permit comes back, so a waiter re-checks rather than polls.
    returned: Notify,
}

#[derive(Default)]
struct State {
    /// `None` until an adapter states one: an unstated ceiling bounds nothing.
    limit: Option<usize>,
    in_flight: usize,
}

impl core::fmt::Debug for RequestGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let state = self.0.state.lock().expect("request gate mutex poisoned");
        f.debug_struct("RequestGate")
            .field("limit", &state.limit)
            .field("in_flight", &state.in_flight)
            .finish()
    }
}

impl RequestGate {
    /// A gate that bounds nothing until an adapter states a ceiling.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lowers the ceiling to `limit`, if that is stricter than whatever it already is.
    ///
    /// Called by an adapter once, as it connects, with what its own server allows: Graph's
    /// documented per-mailbox 4, Gmail's measured 20, the `maxConcurrentRequests` a JMAP
    /// session states (RFC 8620 §2). A `0` is read as `1` for the reason
    /// [`ConnectionInfo::with_concurrent_fetches`](engine_provider::ConnectionInfo::with_concurrent_fetches)
    /// clamps one away: a zero ceiling parks every request the account will ever make.
    ///
    /// Narrowing while requests are in flight is safe — the ones already admitted finish,
    /// and the gate settles at the new ceiling as they return.
    pub fn narrow_to(&self, limit: usize) {
        let limit = limit.max(1);
        let mut state = self.0.state.lock().expect("request gate mutex poisoned");
        if state.limit.is_none_or(|current| limit < current) {
            state.limit = Some(limit);
        }
    }

    /// The ceiling in force, or `None` while no adapter has stated one.
    #[must_use]
    pub fn limit(&self) -> Option<usize> {
        self.0
            .state
            .lock()
            .expect("request gate mutex poisoned")
            .limit
    }

    /// Waits for a lane, and holds it until the returned permit is dropped.
    ///
    /// `send_retrying` holds one across a whole call, the backoff sleep included: a request
    /// waiting out a `Retry-After` is still a request in progress, and handing its lane to
    /// another request while the server is asking for less is how one throttle becomes
    /// several.
    pub async fn acquire(&self) -> GatePermit<'_> {
        loop {
            // Registered *before* the check, and enabled, so a permit returned between the
            // check and the await is not a wakeup this waiter sleeps through.
            let returned = self.0.returned.notified();
            tokio::pin!(returned);
            returned.as_mut().enable();
            {
                let mut state = self.0.state.lock().expect("request gate mutex poisoned");
                if state.limit.is_none_or(|limit| state.in_flight < limit) {
                    state.in_flight += 1;
                    return GatePermit { gate: &self.0 };
                }
            }
            returned.await;
        }
    }
}

/// One lane of an account's [`RequestGate`], returned to it on drop — including the drop an
/// unwind does, so a request that dies mid-flight does not cost the account a lane.
pub struct GatePermit<'a> {
    gate: &'a Inner,
}

impl core::fmt::Debug for GatePermit<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GatePermit").finish_non_exhaustive()
    }
}

impl Drop for GatePermit<'_> {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .expect("request gate mutex poisoned")
            .in_flight -= 1;
        self.gate.returned.notify_one();
    }
}
