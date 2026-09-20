//! What [`RequestGate`] guarantees: a ceiling that holds across every provider sharing it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::RequestGate;

/// Runs `tasks` bodies through `gate` at once and reports the highest number that were
/// inside it simultaneously — which is the only property the gate exists to bound.
async fn peak_concurrency(gate: &RequestGate, tasks: usize) -> usize {
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..tasks {
        let gate = gate.clone();
        let live = Arc::clone(&live);
        let peak = Arc::clone(&peak);
        handles.push(tokio::spawn(async move {
            let _permit = gate.acquire().await;
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            // Long enough that every task that *could* overlap does.
            tokio::time::sleep(Duration::from_millis(20)).await;
            live.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for handle in handles {
        handle.await.expect("task");
    }
    peak.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_narrowed_gate_never_lets_more_than_its_limit_run_at_once() {
    let gate = RequestGate::new();
    gate.narrow_to(4);
    assert_eq!(peak_concurrency(&gate, 20).await, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ungoverned_gate_bounds_nothing() {
    // The default a host that wired no gate gets: today's behaviour, not a silent
    // serialization of every request it makes.
    let gate = RequestGate::new();
    assert!(peak_concurrency(&gate, 8).await > 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clones_share_one_ceiling() {
    // The whole point: an account's providers are handed clones, and the server counts
    // their requests together. A clone that bounded itself would be four gates of four.
    let gate = RequestGate::new();
    gate.narrow_to(2);
    let (a, b) = (gate.clone(), gate.clone());
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for gate in [a, b] {
        for _ in 0..6 {
            let (gate, live, peak) = (gate.clone(), Arc::clone(&live), Arc::clone(&peak));
            handles.push(tokio::spawn(async move {
                let _permit = gate.acquire().await;
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
    }
    for handle in handles {
        handle.await.expect("task");
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn narrowing_only_ever_narrows() {
    // Two adapters on one account each state what their own server allows, in whatever
    // order they happen to connect. The account is bound by the strictest of them, and a
    // later, looser statement must not raise a ceiling an earlier one lowered.
    let gate = RequestGate::new();
    gate.narrow_to(4);
    gate.narrow_to(20);
    assert_eq!(gate.limit(), Some(4));
    gate.narrow_to(2);
    assert_eq!(gate.limit(), Some(2));
}

#[tokio::test]
async fn a_zero_limit_is_refused_rather_than_deadlocking_the_account() {
    // `ConnectionInfo::with_concurrent_fetches` clamps a zero away for the same reason;
    // a gate that accepted one would park every request the account ever makes.
    let gate = RequestGate::new();
    gate.narrow_to(0);
    assert_eq!(gate.limit(), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permit_is_returned_when_its_holder_unwinds() {
    // A request that fails mid-flight must not cost the account a lane for the rest of
    // the session. The permit is a guard, so the unwind returns it.
    let gate = RequestGate::new();
    gate.narrow_to(1);
    let panicking = {
        let gate = gate.clone();
        tokio::spawn(async move {
            let _permit = gate.acquire().await;
            panic!("a request that died holding a lane");
        })
    };
    assert!(panicking.await.is_err());
    // The lane came back, so this does not hang.
    let permit = tokio::time::timeout(Duration::from_secs(5), gate.acquire()).await;
    assert!(permit.is_ok(), "the panicking holder kept its lane");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narrowing_under_load_settles_at_the_new_ceiling() {
    let gate = RequestGate::new();
    gate.narrow_to(8);
    // Narrowed while requests are already in flight: the ones already admitted finish,
    // and the gate settles at the new ceiling rather than admitting to the old one.
    let narrowing = {
        let gate = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            gate.narrow_to(2);
        })
    };
    let peak = peak_concurrency(&gate, 24).await;
    narrowing.await.expect("task");
    assert!(peak <= 8, "admitted {peak}, over the ceiling it started at");
    assert_eq!(gate.limit(), Some(2));
    // And from here on the new ceiling is the one that holds.
    assert_eq!(peak_concurrency(&gate, 12).await, 2);
}

#[tokio::test]
async fn a_gate_and_its_permits_describe_themselves_without_leaking_anything() {
    // `Debug` on these reaches a diagnostic dump, so it has to render — and it must carry the
    // ceiling and the load, which is the whole question anyone printing it is asking.
    let gate = RequestGate::new();
    gate.narrow_to(3);
    let rendered = format!("{gate:?}");
    assert!(rendered.contains('3'), "the ceiling is missing: {rendered}");
    assert!(
        rendered.contains("in_flight"),
        "the load is missing: {rendered}"
    );

    let permit = gate.acquire().await;
    assert_eq!(format!("{permit:?}"), "GatePermit { .. }");
    assert!(format!("{gate:?}").contains("in_flight: 1"));
}
