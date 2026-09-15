//! Targeted-claim cases: reaching one op by id, and why a claim refused.

use core::time::Duration;

use engine_core::write::{PendingOpId, PendingOutcome};

use super::super::{acct, lease_request, pending_op, pk};
use crate::{
    lease::ManualClock,
    outbox::{ClaimRejection, PendingOpClaim, PendingOpState},
    read::StoreRead,
    store::Store,
};

/// `claim_pending_op` leases the op it names however much older work is runnable.
///
/// The batch claim is ordered and bounded, so reaching a *particular* op through it
/// works only while the account's runnable set stays under the batch size. An inline
/// driver resolving the op it just enqueued needs that op, not the head of the queue.
pub(in crate::contract) async fn a_targeted_claim_reaches_an_op_behind_a_backlog<
    S: Store + StoreRead,
>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-targeted");
    let mut older = Vec::new();
    for i in 0..40 {
        older.push(
            store
                .enqueue_pending_op(
                    account.clone(),
                    pending_op(&format!("older-{i}"), &format!("res-{i}")),
                )
                .await
                .unwrap(),
        );
    }
    let mine = store
        .enqueue_pending_op(account.clone(), pending_op("mine", "res-mine"))
        .await
        .unwrap();

    let claim = store
        .claim_pending_op(account.clone(), mine, lease_request("worker", 30))
        .await
        .unwrap();
    let PendingOpClaim::Leased(leased) = claim else {
        panic!("the named op must be leased, not refused: {claim:?}");
    };
    assert_eq!(leased.id, mine);
    assert_eq!(
        store.pending_op_state(mine).await.unwrap(),
        Some(PendingOpState::InFlight)
    );

    // It leases that op alone: the backlog is untouched, so nothing is leased to a
    // worker that will never resolve it.
    for id in [older[0], older[19]] {
        assert_eq!(
            store.pending_op_state(id).await.unwrap(),
            Some(PendingOpState::Pending)
        );
    }
}

/// A refused targeted claim says which condition refused it.
///
/// A caller waits out a busy resource and does not wait out an unmet dependency, so
/// one opaque "not claimable" cannot serve both.
pub(in crate::contract) async fn a_targeted_claim_names_why_it_refused<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-refusal");

    // Unknown: no such op.
    assert_eq!(
        store
            .claim_pending_op(
                account.clone(),
                PendingOpId::new(9_999),
                lease_request("w", 30)
            )
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Unknown)
    );

    // Busy: another op holds this one's resource under a live lease.
    let holder = store
        .enqueue_pending_op(account.clone(), pending_op("holder", "res-shared"))
        .await
        .unwrap();
    let waiter = store
        .enqueue_pending_op(account.clone(), pending_op("waiter", "res-shared"))
        .await
        .unwrap();
    let held = store
        .claim_pending_op(account.clone(), holder, lease_request("w", 30))
        .await
        .unwrap();
    let PendingOpClaim::Leased(held) = held else {
        panic!("the holder must lease")
    };
    assert_eq!(
        store
            .claim_pending_op(account.clone(), waiter, lease_request("w", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Busy)
    );

    // The op itself, already leased and live, is equally busy.
    assert_eq!(
        store
            .claim_pending_op(account.clone(), holder, lease_request("w2", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Busy)
    );

    // DependencyUnmet: the dependency has not reached terminal success.
    let mut dependent = pending_op("dependent", "res-dependent");
    dependent.depends_on.push(holder);
    let dependent_id = store
        .enqueue_pending_op(account.clone(), dependent)
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_pending_op(account.clone(), dependent_id, lease_request("w", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::DependencyUnmet)
    );

    // Settled: a terminal outcome is recorded, so there is nothing left to run.
    store
        .mark_pending_op(
            &held.lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_pending_op(account.clone(), holder, lease_request("w", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Settled)
    );

    // Resolving it frees both the resource and the dependency.
    for id in [waiter, dependent_id] {
        let claim = store
            .claim_pending_op(account.clone(), id, lease_request("w", 30))
            .await
            .unwrap();
        assert!(
            matches!(claim, PendingOpClaim::Leased(ref l) if l.id == id),
            "op {id:?} must be claimable once the holder settled, got {claim:?}"
        );
    }
}

/// A dead lease holds nothing, and a dependency that does not exist is never met.
///
/// The first is how an account with a backlog of abandoned in-flight ops recovers
/// without surgery: those ops still read `InFlight`, but their leases expired, so they
/// serialize against nothing, a new write to the same resource runs, and the abandoned
/// op itself is claimable again under a fresh token.
pub(in crate::contract) async fn a_dead_lease_holds_no_resource<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-dead-lease");
    let abandoned = store
        .enqueue_pending_op(account.clone(), pending_op("abandoned", "res-shared"))
        .await
        .unwrap();
    let later = store
        .enqueue_pending_op(account.clone(), pending_op("later", "res-shared"))
        .await
        .unwrap();
    let solo = store
        .enqueue_pending_op(account.clone(), pending_op("solo", "res-solo"))
        .await
        .unwrap();

    // Claimed and never resolved: the shape a driver leaves behind when it dies
    // mid-write.
    let claim = store
        .claim_pending_op(account.clone(), abandoned, lease_request("gone", 30))
        .await
        .unwrap();
    assert!(matches!(claim, PendingOpClaim::Leased(_)));
    let dead = store
        .claim_pending_op(account.clone(), solo, lease_request("gone", 30))
        .await
        .unwrap();
    let PendingOpClaim::Leased(dead) = dead else {
        panic!("the solo op must lease")
    };
    assert_eq!(
        store
            .claim_pending_op(account.clone(), later, lease_request("w", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::Busy),
        "while the lease is live the resource is genuinely held"
    );

    clock.advance(Duration::from_secs(90)); // the abandoned op's lease expires

    let claim = store
        .claim_pending_op(account.clone(), later, lease_request("w", 30))
        .await
        .unwrap();
    assert!(
        matches!(claim, PendingOpClaim::Leased(ref l) if l.id == later),
        "a dead lease must not hold the resource, got {claim:?}"
    );
    // The abandoned op is still there, still unresolved: recovering the resource is
    // not the same as discarding the intent.
    assert_eq!(
        store.pending_op_state(abandoned).await.unwrap(),
        Some(PendingOpState::InFlight)
    );
    // And such an op is claimable again itself, under a token that fences out the
    // driver that walked away with the old one. This is what unwedges an account:
    // nothing has to notice the abandoned op, the next attempt simply takes it.
    let retaken = store
        .claim_pending_op(account.clone(), solo, lease_request("w2", 30))
        .await
        .unwrap();
    let PendingOpClaim::Leased(retaken) = retaken else {
        panic!("an op whose own lease died must be re-claimable, got {retaken:?}")
    };
    assert_ne!(retaken.lease.token(), dead.lease.token());

    // A dependency naming an op this account's outbox does not hold is unmet, not met
    // by default.
    let mut orphan = pending_op("orphan", "res-orphan");
    orphan.depends_on.push(PendingOpId::new(8_888));
    let orphan_id = store
        .enqueue_pending_op(account.clone(), orphan)
        .await
        .unwrap();
    assert_eq!(
        store
            .claim_pending_op(account, orphan_id, lease_request("w", 30))
            .await
            .unwrap(),
        PendingOpClaim::Refused(ClaimRejection::DependencyUnmet)
    );
}
