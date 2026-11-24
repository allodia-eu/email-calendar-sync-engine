//! What the last probe of an account's ManageSieve service found, and when a dial may probe it
//! again (`crate::managesieve::reach`).
//!
//! The outcome is one byte, so a reader never sees the reply offered by one probe beside the
//! availability of another. A probe that fails starts a backoff ([`PROBE_BACKOFF`]): until it
//! passes no dial probes, so a blackholed endpoint costs a dial at most one probe deadline per
//! backoff. A probe taken and then dropped unfinished (its dial cancelled) is wanted again.

use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicU8, Ordering},
};

use tokio::time::{Duration, Instant};

/// How long after a failed probe no dial probes again, whatever wants it.
pub(crate) const PROBE_BACKOFF: Duration = Duration::from_mins(5);

/// The last probe succeeded: the rules are offered.
const OFFERED: u8 = 1;
/// The last probe found `vacation` listed: the reply is offered too.
const REPLIES: u8 = 2;
/// A dial is to probe again: after a failure that may pass, or `invalidate()`.
const WANTED: u8 = 4;

/// The probe state of one account's ManageSieve service.
#[derive(Debug)]
pub(crate) struct ProbeState {
    bits: AtomicU8,
    /// No probe before this, after a failure.
    not_before: Mutex<Option<Instant>>,
}

impl ProbeState {
    /// Offered, with the reply where `replies`, as if a probe had just succeeded.
    pub(crate) fn offered_with(replies: bool) -> Self {
        Self {
            bits: AtomicU8::new(OFFERED | if replies { REPLIES } else { 0 }),
            not_before: Mutex::new(None),
        }
    }

    /// What the last probe found: `None` where it failed, else whether the reply is offered.
    pub(crate) fn offered(&self) -> Option<bool> {
        let bits = self.bits.load(Ordering::SeqCst);
        (bits & OFFERED != 0).then_some(bits & REPLIES != 0)
    }

    /// Records a probe: offered, with the reply where `Some(true)`, or failed, starting the
    /// backoff, and whether a dial is to probe again once the backoff allows (`retry`).
    pub(crate) fn record(&self, offered: Option<bool>, retry: bool) {
        *self
            .not_before
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            offered.is_none().then(|| Instant::now() + PROBE_BACKOFF);
        let bits = match offered {
            Some(true) => OFFERED | REPLIES,
            Some(false) => OFFERED,
            None => 0,
        } | if retry { WANTED } else { 0 };
        self.bits.store(bits, Ordering::SeqCst);
    }

    /// Asks a later dial to probe (`ImapAccount::invalidate`), once the backoff allows.
    pub(crate) fn want(&self) {
        self.bits.fetch_or(WANTED, Ordering::SeqCst);
    }

    /// Takes the want for one dial, where a probe is wanted and the backoff allows: one dial
    /// probes, not every one at once. Dropped unrecorded, the want comes back.
    pub(crate) fn take(&self) -> Option<Taken<'_>> {
        let backing_off = self
            .not_before
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|not_before| Instant::now() < not_before);
        let wanted = !backing_off && self.bits.fetch_and(!WANTED, Ordering::SeqCst) & WANTED != 0;
        wanted.then_some(Taken {
            state: self,
            recorded: false,
        })
    }
}

/// A probe one dial has taken on: recorded when it finishes, wanted again if it never does.
pub(crate) struct Taken<'a> {
    state: &'a ProbeState,
    recorded: bool,
}

impl Taken<'_> {
    /// Records the probe's outcome ([`ProbeState::record`]).
    pub(crate) fn record(mut self, offered: Option<bool>, retry: bool) {
        self.recorded = true;
        self.state.record(offered, retry);
    }
}

impl Drop for Taken<'_> {
    fn drop(&mut self) {
        if !self.recorded {
            self.state.want();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_load_answers_whether_the_rules_and_the_reply_are_offered() {
        let state = ProbeState::offered_with(true);
        assert_eq!(state.offered(), Some(true));
        state.record(Some(false), false);
        assert_eq!(state.offered(), Some(false));
        state.record(None, true);
        assert_eq!(state.offered(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_failure_no_dial_probes_until_the_backoff_has_passed() {
        let state = ProbeState::offered_with(true);
        state.record(None, true);
        assert!(state.take().is_none(), "backing off");
        state.want();
        assert!(
            state.take().is_none(),
            "invalidate waits for the backoff too"
        );
        tokio::time::advance(PROBE_BACKOFF).await;
        let taken = state.take().expect("wanted, and the backoff has passed");
        assert!(state.take().is_none(), "one dial probes, not every one");
        taken.record(Some(true), false);
        assert!(state.take().is_none(), "a success wants nothing");
        state.want();
        assert!(state.take().is_some(), "no backoff after a success");
    }

    #[test]
    fn a_probe_dropped_unfinished_is_wanted_again() {
        let state = ProbeState::offered_with(false);
        state.want();
        drop(state.take().expect("wanted"));
        assert!(state.take().is_some(), "wanted again");
    }
}
