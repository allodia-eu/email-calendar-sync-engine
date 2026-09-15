//! Outbox contract cases: enqueue (idempotent), claim (dependency/resource
//! filtering, op-lease expiry, backoff), mark, retry parking, cancellation, and
//! the queue read.

mod claim;
mod lifecycle;
mod queue;

pub(super) use self::{
    claim::{
        a_dead_lease_holds_no_resource, a_targeted_claim_names_why_it_refused,
        a_targeted_claim_reaches_an_op_behind_a_backlog,
    },
    lifecycle::{
        claim_filters_dependencies_and_resources, claim_respects_limit, enqueue_is_idempotent,
        expired_op_lease_is_rejected, outcomes_record_failure_and_ambiguity,
        unknown_op_is_rejected_and_stateless,
    },
    queue::{
        a_cancelled_op_is_never_attempted, a_queue_read_lists_what_has_not_settled,
        a_retryable_failure_comes_back_when_its_backoff_elapses,
        a_retryable_failure_settles_once_its_attempts_run_out,
    },
};
