//! Outbox contract cases: enqueue (idempotent), claim (dependency/resource
//! filtering, op-lease expiry), and mark.

use core::time::Duration;

use engine_core::{
    error::FailureClass,
    write::{PendingOpId, PendingOutcome},
};

use super::{acct, lease_request, pending_op, pk};
use crate::{
    error::StoreError,
    lease::{FenceToken, ManualClock, OpLease, WorkerId},
    outbox::{ClaimRejection, PendingOpClaim, PendingOpState},
    store::{Store, StoreRead},
};

/// `mark_pending_op` under an expired op lease is rejected after the op was
/// re-claimed; the new lease succeeds.
pub(super) async fn expired_op_lease_is_rejected<S: Store + StoreRead>(
    store: &S,
    clock: &ManualClock,
) {
    let account = acct("acct-op-expiry");
    let op_id = store
        .enqueue_pending_op(account.clone(), pending_op("send-1", "draft-1"))
        .await
        .unwrap();

    let claimed_old = store
        .claim_pending_ops(account.clone(), lease_request("worker-old", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed_old.len(), 1);
    let old_lease = claimed_old[0].lease.clone();

    clock.advance(Duration::from_secs(90)); // the op lease expires
    let claimed_new = store
        .claim_pending_ops(account.clone(), lease_request("worker-new", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed_new.len(), 1);
    let new_lease = claimed_new[0].lease.clone();
    assert_ne!(old_lease.token(), new_lease.token());

    let rejected = store
        .mark_pending_op(
            &old_lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-1"),
            },
        )
        .await
        .expect_err("stale op lease must be rejected");
    assert_eq!(rejected, StoreError::StaleLease);

    store
        .mark_pending_op(
            &new_lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-1"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.pending_op_state(op_id).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// `claim_pending_ops` never returns an op with unmet `depends_on`, nor two ops
/// sharing a `resource_key`.
pub(super) async fn claim_filters_dependencies_and_resources<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-claim");
    let first_id = store
        .enqueue_pending_op(account.clone(), pending_op("first", "resource-x"))
        .await
        .unwrap();
    let mut dependent = pending_op("second", "resource-y");
    dependent.depends_on.push(first_id);
    let second_id = store
        .enqueue_pending_op(account.clone(), dependent)
        .await
        .unwrap();
    let third_id = store
        .enqueue_pending_op(account.clone(), pending_op("third", "resource-x"))
        .await
        .unwrap();

    // Only the first op runs: the second's dependency is unmet, the third
    // collides on `resource-x`.
    let round_one = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert_eq!(
        round_one.iter().map(|l| l.id).collect::<Vec<_>>(),
        vec![first_id]
    );

    // While the first op holds `resource-x` in flight, a second claim returns
    // nothing: the third op is blocked by the busy resource, the second by its
    // still-unmet dependency.
    let blocked = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert!(blocked.is_empty());

    store
        .mark_pending_op(
            &round_one[0].lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server"),
            },
        )
        .await
        .unwrap();

    // Now the second (dependency satisfied) and third (resource free) both run.
    let round_two = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    let mut got: Vec<PendingOpId> = round_two.iter().map(|l| l.id).collect();
    got.sort();
    let mut want = vec![second_id, third_id];
    want.sort();
    assert_eq!(got, want);
}

/// Re-enqueuing a duplicate idempotency key returns the original id and creates
/// no second op.
pub(super) async fn enqueue_is_idempotent<S: Store + StoreRead>(store: &S, _clock: &ManualClock) {
    let account = acct("acct-idempotent");
    let first = store
        .enqueue_pending_op(account.clone(), pending_op("dup", "resource"))
        .await
        .unwrap();
    let again = store
        .enqueue_pending_op(account.clone(), pending_op("dup", "resource"))
        .await
        .unwrap();
    assert_eq!(first, again);
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 30), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, first);
}

/// `mark_pending_op` records the failure and ambiguous outcomes distinctly, not
/// only success — the outbox state machine's whole point.
pub(super) async fn outcomes_record_failure_and_ambiguity<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-outcomes");
    // Two ops on separate resources, so both are runnable in one claim.
    store
        .enqueue_pending_op(account.clone(), pending_op("fail-1", "res-fail"))
        .await
        .unwrap();
    store
        .enqueue_pending_op(account.clone(), pending_op("amb-1", "res-amb"))
        .await
        .unwrap();
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 300), 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);

    for leased in &claimed {
        let is_failure = leased.op.idempotency_key.as_str() == "fail-1";
        let outcome = if is_failure {
            PendingOutcome::Failed {
                class: FailureClass::Retryable,
                retry_after: None,
            }
        } else {
            PendingOutcome::NeedsConfirmation {
                detail: "post-DATA timeout".to_owned(),
            }
        };
        store.mark_pending_op(&leased.lease, outcome).await.unwrap();
        let expected = if is_failure {
            PendingOpState::Failed
        } else {
            PendingOpState::NeedsConfirmation
        };
        assert_eq!(
            store.pending_op_state(leased.id).await.unwrap(),
            Some(expected)
        );
    }
}

/// An op id with no row has no state and cannot be marked: a lease naming it is
/// rejected as stale rather than silently applied.
pub(super) async fn unknown_op_is_rejected_and_stateless<S: Store + StoreRead>(
    store: &S,
    _clock: &ManualClock,
) {
    let account = acct("acct-unknown-op");
    let missing = PendingOpId::new(4_242);
    assert_eq!(store.pending_op_state(missing).await.unwrap(), None);

    // A lease minted for an op that never persisted (e.g. a resurrected worker
    // holding a stale handle) must be rejected, not applied.
    let lease = OpLease::new(
        account,
        missing,
        FenceToken::initial().bump(),
        WorkerId::new("ghost"),
        "2026-01-01T00:00:00Z".parse().expect("valid instant"),
    );
    let rejected = store
        .mark_pending_op(
            &lease,
            PendingOutcome::Succeeded {
                provider_key: pk("server-x"),
            },
        )
        .await
        .expect_err("marking an unknown op must be rejected");
    assert_eq!(rejected, StoreError::StaleLease);
}

/// `claim_pending_ops` returns at most `limit` ops even when more are runnable.
pub(super) async fn claim_respects_limit<S: Store + StoreRead>(store: &S, _clock: &ManualClock) {
    let account = acct("acct-limit");
    store
        .enqueue_pending_op(account.clone(), pending_op("a", "res-a"))
        .await
        .unwrap();
    store
        .enqueue_pending_op(account.clone(), pending_op("b", "res-b"))
        .await
        .unwrap();
    // Both are runnable (distinct resources, no deps), but the limit caps the batch.
    let claimed = store
        .claim_pending_ops(account.clone(), lease_request("worker", 300), 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
}

/// `claim_pending_op` leases the op it names however much older work is runnable.
///
/// The batch claim is ordered and bounded, so reaching a *particular* op through it
/// works only while the account's runnable set stays under the batch size. An inline
/// driver resolving the op it just enqueued needs that op, not the head of the queue.
pub(super) async fn a_targeted_claim_reaches_an_op_behind_a_backlog<S: Store + StoreRead>(
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
pub(super) async fn a_targeted_claim_names_why_it_refused<S: Store + StoreRead>(
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
pub(super) async fn a_dead_lease_holds_no_resource<S: Store + StoreRead>(
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
