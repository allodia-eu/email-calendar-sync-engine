//! What a paced [`RequestGate`] guarantees: no window of the stated length carries more than the
//! stated cost, a sustained load still runs close to it, and requests are served in the order
//! they asked.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::time::Instant;

use crate::RequestGate;

const WINDOW: Duration = Duration::from_mins(1);

/// Spends `costs` one after another through `gate`, returning when each went out and what it
/// cost.
async fn spend_all(
    gate: &RequestGate,
    costs: impl IntoIterator<Item = u32>,
) -> Vec<(Instant, u32)> {
    let mut sent = Vec::new();
    for cost in costs {
        gate.spend(cost).await;
        sent.push((Instant::now(), cost));
    }
    sent
}

/// The most any window of length `window` carried, over every window starting at a send.
fn busiest_window(sent: &[(Instant, u32)], window: Duration) -> u32 {
    sent.iter()
        .map(|(start, _)| {
            sent.iter()
                .filter(|(at, _)| *at >= *start && *at < *start + window)
                .map(|(_, cost)| cost)
                .sum()
        })
        .max()
        .unwrap_or(0)
}

#[tokio::test(start_paused = true)]
async fn an_unpaced_gate_charges_nothing_and_waits_for_nothing() {
    let gate = RequestGate::new();
    let started = Instant::now();

    spend_all(&gate, [60; 1000]).await;

    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(gate.pace(), None);
}

#[tokio::test(start_paused = true)]
async fn no_window_carries_more_than_the_stated_cost() {
    // Gmail's own shape: a quota of units per minute, spent by calls of different prices, a
    // listing at 5, a message at 20, a body at 60. However they mix, no minute may go over,
    // because the server refuses the call that would.
    let gate = RequestGate::new();
    gate.pace_to(6000, WINDOW);

    let sent = spend_all(
        &gate,
        [5, 20, 20, 20, 60, 20, 20].into_iter().cycle().take(2000),
    )
    .await;

    let busiest = busiest_window(&sent, WINDOW);
    assert!(busiest <= 6000, "a minute carried {busiest} units");
}

#[tokio::test(start_paused = true)]
async fn a_sustained_load_still_runs_close_to_the_stated_rate() {
    // The opposite failure, and just as real: a pace so cautious that a long backfill takes
    // twice as long as the server would allow.
    let gate = RequestGate::new();
    gate.pace_to(6000, WINDOW);
    let started = Instant::now();

    let sent = spend_all(&gate, [20; 1500]).await;

    let minutes = started.elapsed().as_secs_f64() / 60.0;
    let total: u32 = sent.iter().map(|(_, cost)| cost).sum();
    let per_minute = f64::from(total) / minutes;
    assert!(
        per_minute >= 5000.0,
        "sustained only {per_minute:.0} units a minute"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rested_gate_lets_a_short_burst_through_at_once() {
    // Opening one message on an idle account must not wait on the pace.
    let gate = RequestGate::new();
    gate.pace_to(6000, WINDOW);
    let started = Instant::now();

    spend_all(&gate, [60; 5]).await;

    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn requests_are_served_in_the_order_they_asked() {
    // A body fetch at 60 units queued behind a backfill's 20-unit gets must not wait for a
    // gap the gets keep filling: whoever asked first goes first, whatever it costs.
    let gate = RequestGate::new();
    gate.pace_to(600, WINDOW);
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    // A get, a body, two gets, over and over: the prices a backfill and a body warm mix.
    let costs = [20, 60, 20, 20].into_iter().cycle().take(40);
    for (asked, cost) in costs.enumerate() {
        let (gate, order) = (gate.clone(), Arc::clone(&order));
        tasks.push(tokio::spawn(async move {
            gate.spend(cost).await;
            order.lock().expect("order").push(asked);
        }));
        // Each task reserves on its first poll; yielding here fixes the order they ask in.
        tokio::task::yield_now().await;
    }
    for task in tasks {
        task.await.expect("task");
    }

    let served = order.lock().expect("order").clone();
    assert_eq!(served, (0..40).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn clones_share_one_budget() {
    // An account's providers, and a second core's, are handed clones. Two budgets of 6,000
    // against one user's 6,000 is the refusal this exists to prevent.
    let gate = RequestGate::new();
    gate.pace_to(6000, WINDOW);
    let (a, b) = (gate.clone(), gate.clone());

    let (one, two) = tokio::join!(spend_all(&a, [20; 600]), spend_all(&b, [20; 600]));

    let mut sent = one;
    sent.extend(two);
    sent.sort_by_key(|(at, _)| *at);
    let busiest = busiest_window(&sent, WINDOW);
    assert!(
        busiest <= 6000,
        "two clones together put {busiest} units in a minute"
    );
}

#[tokio::test]
async fn pacing_only_ever_slows() {
    // Two adapters on one account may each state a pace; the account is held to the stricter,
    // whatever order they connect in.
    let gate = RequestGate::new();
    gate.pace_to(6000, WINDOW);
    gate.pace_to(12_000, WINDOW);
    assert_eq!(gate.pace(), Some((6000, WINDOW)));
    gate.pace_to(50, Duration::from_secs(1));
    assert_eq!(gate.pace(), Some((50, Duration::from_secs(1))));
}

#[tokio::test(start_paused = true)]
async fn a_call_dearer_than_the_burst_still_goes_out() {
    // A send costs 100 units; a pace whose burst is smaller than that must not park it forever.
    let gate = RequestGate::new();
    gate.pace_to(500, WINDOW);

    let sent = tokio::time::timeout(Duration::from_mins(10), spend_all(&gate, [100; 3])).await;

    assert!(sent.is_ok(), "a call dearer than the burst never went out");
}

#[tokio::test(start_paused = true)]
async fn a_call_that_costs_nothing_never_waits_behind_the_budget() {
    // Google bills Calendar and People under quotas of their own, so an account whose Gmail
    // budget is minutes behind must still reach its calendar at once.
    let gate = RequestGate::new();
    gate.pace_to(600, WINDOW);
    // Fifty body fetches queued at once: minutes of budget reserved ahead.
    let queued: Vec<_> = (0..50)
        .map(|_| {
            let gate = gate.clone();
            tokio::spawn(async move { gate.spend(60).await })
        })
        .collect();
    tokio::task::yield_now().await;
    let behind = Instant::now();

    gate.spend(0).await;

    assert_eq!(behind.elapsed(), Duration::ZERO);
    for task in queued {
        task.abort();
    }
}
