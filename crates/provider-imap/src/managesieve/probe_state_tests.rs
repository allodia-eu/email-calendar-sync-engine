//! What the probe state offers, when a probe may start, and what a call waits for.

use super::*;

fn unprobed() -> Arc<ProbeState> {
    Arc::new(ProbeState::unprobed())
}

#[test]
fn what_a_landed_probe_found_is_what_is_offered() {
    let state = unprobed();
    state.begin().unwrap().record(Some(true), false);
    assert_eq!(state.offered(), Offer::Found { reply: true });
    state.want();
    state.take().unwrap().record(Some(false), false);
    assert_eq!(state.offered(), Offer::Found { reply: false });
    assert!(state.offered().rules() && !state.offered().reply());
    state.want();
    state.take().unwrap().record(None, true);
    assert_eq!(state.offered(), Offer::Missing { may_pass: true });
    assert!(!state.offered().rules() && !state.offered().reply());
}

#[tokio::test(start_paused = true)]
async fn only_the_first_probe_offers_both_and_is_waited_for() {
    let state = unprobed();
    let taken = state.begin().expect("none runs");
    assert!(state.begin().is_none(), "one probe at a time");
    assert_eq!(state.offered(), Offer::Probing);
    assert!(state.offered().rules() && state.offered().reply());
    let lands = async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        taken.record(Some(false), false);
    };
    let started = Instant::now();
    let ((), settled) = tokio::join!(lands, state.settled());
    assert_eq!(settled, Offer::Found { reply: false });
    assert_eq!(started.elapsed(), Duration::from_secs(3), "waited for it");
    // A later probe: the last finding stands until it lands, and nothing waits for it.
    state.want();
    let again = state.take().expect("wanted");
    assert_eq!(state.offered(), Offer::Found { reply: false });
    assert_eq!(state.settled().await, Offer::Found { reply: false });
    assert_eq!(started.elapsed(), Duration::from_secs(3), "not waited for");
    again.record(None, false);
    assert_eq!(state.settled().await, Offer::Missing { may_pass: false });
}

#[tokio::test(start_paused = true)]
async fn after_a_failure_no_probe_starts_until_the_backoff_has_passed() {
    let state = unprobed();
    state.begin().unwrap().record(None, true);
    assert!(state.wanted(), "a failure that may pass wants another");
    assert!(state.take().is_none(), "backing off");
    state.want();
    assert!(state.take().is_none(), "invalidate waits for it too");
    tokio::time::advance(PROBE_BACKOFF).await;
    let taken = state.take().expect("wanted, and the backoff has passed");
    assert!(state.take().is_none(), "one probe at a time");
    taken.record(Some(true), false);
    assert!(state.take().is_none(), "a success wants nothing");
    state.want();
    assert!(state.take().is_some(), "no backoff after a success");
    // A definite answer wants nothing more.
    let state = unprobed();
    state.begin().unwrap().record(None, false);
    assert!(!state.wanted());
    assert_eq!(state.offered(), Offer::Missing { may_pass: false });
}

#[test]
fn a_first_probe_dropped_unfinished_offers_nothing_and_is_wanted_again() {
    let state = unprobed();
    drop(state.begin().expect("none runs"));
    assert_eq!(state.offered(), Offer::Missing { may_pass: true });
    assert!(state.take().is_some(), "wanted again, with no backoff");
    // A later one leaves the last finding as it was.
    let state = Arc::new(ProbeState::offered_with(false));
    state.want();
    drop(state.take().expect("wanted"));
    assert_eq!(state.offered(), Offer::Found { reply: false });
    assert!(state.take().is_some(), "wanted again");
}

#[test]
fn a_want_that_comes_while_a_probe_runs_is_for_the_next_one() {
    let state = unprobed();
    let taken = state.begin().expect("none runs");
    state.want();
    taken.record(Some(true), false);
    assert!(state.take().is_some(), "the want was kept");
    // A want before the probe started is the one it served.
    let state = unprobed();
    state.want();
    state.begin().expect("none runs").record(Some(true), false);
    assert!(state.take().is_none());
}

#[test]
fn each_probe_is_newer_than_the_last_and_only_the_running_one_records() {
    let state = unprobed();
    let first = state.begin().unwrap();
    let generation = first.generation();
    first.record(Some(true), false);
    state.want();
    let second = state.take().unwrap();
    assert!(second.generation() > generation);
    // A stale outcome (a generation no longer running) changes nothing.
    state.record(generation, None, false);
    assert_eq!(state.offered(), Offer::Found { reply: true });
    second.record(None, false);
    assert_eq!(state.offered(), Offer::Missing { may_pass: false });
}
