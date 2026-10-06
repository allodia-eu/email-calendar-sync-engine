//! Outbox contract cases: enqueue (idempotent), claim (dependency/resource
//! filtering, op-lease expiry, backoff), mark, retry parking, cancellation, and
//! the queue read.

mod claim;
mod hand_over;
mod interrupted;
mod keeping;
mod keyword_edits;
mod lifecycle;
mod queue;

pub(super) use self::{
    claim::{
        a_dead_lease_holds_no_resource, a_targeted_claim_names_why_it_refused,
        a_targeted_claim_reaches_an_op_behind_a_backlog,
    },
    hand_over::{
        a_confirmation_ends_the_old_attempts_say,
        a_dead_send_that_handed_over_awaits_confirmation_on_every_path,
        a_dead_send_that_never_handed_over_is_retried_on_every_path,
        the_hand_over_is_recorded_under_the_current_lease_only,
    },
    interrupted::{
        a_sent_copy_resolves_a_dead_send_nothing_recovered_yet,
        an_op_the_previous_process_left_in_flight_is_recovered,
    },
    keeping::{
        a_renewed_lease_keeps_a_slow_attempt_its_own,
        a_send_that_failed_stays_listed_until_the_host_acts,
    },
    keyword_edits::{
        a_change_to_a_message_not_yet_held_shows_when_it_arrives,
        a_queued_keyword_change_shows_at_once_and_survives_a_sync,
        an_accepted_change_outlasts_a_pass_that_read_before_it,
        only_a_change_that_will_not_happen_goes_back, queued_changes_compose_and_one_can_drop_out,
    },
    lifecycle::{
        claim_filters_dependencies_and_resources, claim_respects_limit, enqueue_is_idempotent,
        expired_op_lease_is_rejected, outcomes_record_failure_and_ambiguity,
        unknown_op_is_rejected_and_stateless,
    },
    queue::{
        a_cancelled_op_is_never_attempted, a_parked_retry_can_be_hurried,
        a_queue_read_lists_what_has_not_settled,
        a_retryable_failure_comes_back_when_its_backoff_elapses,
        a_retryable_failure_settles_once_its_attempts_run_out,
        the_host_verbs_refuse_what_they_cannot_act_on,
    },
};
