//! What the probes of an account's ManageSieve service have found, whether one is running now, and
//! when the next may start (`crate::managesieve::reach`).
//!
//! The whole state sits behind one `watch` channel, read and changed under its lock, so a reader
//! never sees the reply offered by one probe beside the availability of another, a probe never
//! starts between a failure and the backoff it sets, and a call waiting for the account's first
//! probe wakes when it lands. A probe that fails starts a backoff ([`PROBE_BACKOFF`]): until it
//! passes no probe starts, so a blackholed endpoint is dialled at most once per backoff. A probe
//! dropped unfinished (its task ended with the service) is wanted again.

use std::sync::Arc;

use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

/// How long after a failed probe none starts again, whatever wants it.
pub(crate) const PROBE_BACKOFF: Duration = Duration::from_mins(5);

/// What the account's providers offer of its ManageSieve service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
    /// The account's first probe is running: nothing is known yet, both are offered, and a call
    /// waits for it.
    Probing,
    /// The last probe that landed succeeded: the rules, and the reply where the server listed
    /// `vacation`.
    Found {
        /// Whether `vacation` was listed.
        reply: bool,
    },
    /// No probe has succeeded since the last that failed, or none has landed: neither is offered.
    Missing {
        /// Whether the failure may pass (the network, a timeout, a server closing, a probe that
        /// never landed), as against a definite answer (no `STARTTLS`, not ManageSieve).
        may_pass: bool,
    },
}

impl Offer {
    /// Whether the rules are offered.
    pub(crate) fn rules(self) -> bool {
        matches!(self, Self::Probing | Self::Found { .. })
    }

    /// Whether the reply is offered.
    pub(crate) fn reply(self) -> bool {
        matches!(self, Self::Probing | Self::Found { reply: true })
    }
}

/// What the probes have learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Learned {
    /// No probe has landed yet.
    Nothing,
    /// The last one succeeded.
    Offered { reply: bool },
    /// The last one failed.
    Failed { may_pass: bool },
}

/// The state, changed only under the channel's lock.
#[derive(Debug, Clone, Copy)]
struct Inner {
    learned: Learned,
    /// The generation of the running probe, where one runs.
    running: Option<u64>,
    /// The generation the next probe takes.
    next: u64,
    /// A probe is to start again: after a failure that may pass, or `invalidate()`.
    wanted: bool,
    /// No probe before this, after a failure.
    not_before: Option<Instant>,
}

impl Inner {
    fn offer(&self) -> Offer {
        match (self.learned, self.running) {
            (Learned::Nothing, Some(_)) => Offer::Probing,
            (Learned::Nothing, None) => Offer::Missing { may_pass: true },
            (Learned::Offered { reply }, _) => Offer::Found { reply },
            (Learned::Failed { may_pass }, _) => Offer::Missing { may_pass },
        }
    }
}

/// The probe state of one account's ManageSieve service.
#[derive(Debug)]
pub(crate) struct ProbeState {
    inner: watch::Sender<Inner>,
}

impl ProbeState {
    fn with(learned: Learned, wanted: bool) -> Self {
        Self {
            inner: watch::Sender::new(Inner {
                learned,
                running: None,
                next: 0,
                wanted,
                not_before: None,
            }),
        }
    }

    /// Offered, with the reply where `replies`, as if a probe had just succeeded: a service no
    /// probe watches.
    pub(crate) fn offered_with(replies: bool) -> Self {
        Self::with(Learned::Offered { reply: replies }, false)
    }

    /// Nothing learned yet, a probe wanted: the account's own service, before its first probe.
    pub(crate) fn unprobed() -> Self {
        Self::with(Learned::Nothing, true)
    }

    /// What is offered now.
    pub(crate) fn offered(&self) -> Offer {
        self.inner.borrow().offer()
    }

    /// What is offered once the account's first probe has landed: at once where it has, or where
    /// none runs, else when it lands, which its own deadline bounds (`reach::PROBE_DEADLINE`). A
    /// later probe is not waited for: until it lands, the last one's finding stands.
    pub(crate) async fn settled(&self) -> Offer {
        let mut seen = self.inner.subscribe();
        // The sender is `self`, so the channel cannot close while this waits.
        seen.wait_for(|inner| inner.offer() != Offer::Probing)
            .await
            .map_or(Offer::Missing { may_pass: true }, |inner| inner.offer())
    }

    /// Records what the running probe of generation `generation` found: offered, with the reply
    /// where `Some(true)`; or failed, starting the backoff, and whether that may pass
    /// (`may_pass`), which also asks for a probe once the backoff allows. A want that came while
    /// the probe ran (`invalidate()`) is kept, for a probe after this one.
    fn record(&self, generation: u64, offered: Option<bool>, may_pass: bool) {
        self.inner.send_modify(|inner| {
            if inner.running != Some(generation) {
                return;
            }
            inner.running = None;
            if let Some(reply) = offered {
                inner.learned = Learned::Offered { reply };
                inner.not_before = None;
            } else {
                inner.learned = Learned::Failed { may_pass };
                inner.not_before = Some(Instant::now() + PROBE_BACKOFF);
                inner.wanted |= may_pass;
            }
        });
    }

    /// Asks for a probe on a later dial (`ImapAccount::invalidate`), once the backoff allows.
    pub(crate) fn want(&self) {
        self.inner.send_if_modified(|inner| {
            let was = inner.wanted;
            inner.wanted = true;
            !was
        });
    }

    /// Whether a probe is wanted, whatever the backoff says.
    #[cfg(test)]
    pub(crate) fn wanted(&self) -> bool {
        self.inner.borrow().wanted
    }

    /// Takes the want for one probe, where a probe is wanted, none runs and the backoff allows:
    /// one dial starts a probe, not every one at once. Dropped unrecorded, the want comes back.
    pub(crate) fn take(self: &Arc<Self>) -> Option<Taken> {
        let mut generation = None;
        self.inner.send_if_modified(|inner| {
            let backing_off = inner.not_before.is_some_and(|at| Instant::now() < at);
            if inner.running.is_some() || !inner.wanted || backing_off {
                return false;
            }
            generation = Some(start(inner));
            // Waiting calls care only about a first probe; anyone else sees nothing change.
            inner.learned == Learned::Nothing
        });
        generation.map(|generation| Taken {
            state: Arc::clone(self),
            generation,
            recorded: false,
        })
    }

    /// Starts a probe whatever the backoff says, unless one runs: the account's first, at
    /// connect.
    pub(crate) fn begin(self: &Arc<Self>) -> Option<Taken> {
        let mut generation = None;
        self.inner.send_if_modified(|inner| {
            if inner.running.is_some() {
                return false;
            }
            generation = Some(start(inner));
            true
        });
        generation.map(|generation| Taken {
            state: Arc::clone(self),
            generation,
            recorded: false,
        })
    }
}

/// Marks a probe running, taking any want, and answers its generation.
fn start(inner: &mut Inner) -> u64 {
    let generation = inner.next;
    inner.next += 1;
    inner.running = Some(generation);
    inner.wanted = false;
    generation
}

/// A running probe: recorded when it finishes, wanted again if it never does.
pub(crate) struct Taken {
    state: Arc<ProbeState>,
    generation: u64,
    recorded: bool,
}

impl Taken {
    /// Which probe this is: a later one has a higher generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Records the probe's outcome ([`ProbeState::record`]).
    pub(crate) fn record(mut self, offered: Option<bool>, may_pass: bool) {
        self.recorded = true;
        self.state.record(self.generation, offered, may_pass);
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        if !self.recorded {
            let generation = self.generation;
            self.state.inner.send_modify(|inner| {
                if inner.running == Some(generation) {
                    inner.running = None;
                    inner.wanted = true;
                }
            });
        }
    }
}

#[cfg(test)]
#[path = "probe_state_tests.rs"]
mod tests;
