//! Scope-keyed contract cases: claim, apply (delta and snapshot), reconcile,
//! maintenance, and release.

mod apply;
mod keyword_change;
mod lease;
mod mail;
mod occurrences;
mod read;

pub(super) use self::{
    apply::{
        container_and_member_scopes_are_independent, reconciliation_resolves_matching_op,
        reconciliation_skips_regressed_op, replay_is_idempotent, snapshot_tombstones_only_absent,
        streaming_page_keeps_cursor,
    },
    keyword_change::{
        keyword_change_for_an_unknown_message_writes_nothing,
        keyword_change_moves_flags_and_leaves_the_rest,
    },
    lease::{
        abandon_sync_leases_preserves_cursor_and_fences_old_worker, maintenance_is_lease_gated,
        release_with_stale_token_is_noop, scope_lease_is_exclusive_until_released,
        stale_lease_is_rejected,
    },
    mail::{
        list_mail_by_keys_resolves_named_messages, list_mail_merges_accounts_into_one_order,
        list_mail_on_threads_gathers_only_the_named_threads,
        list_mail_orders_by_date_and_excludes_tombstones,
    },
    occurrences::{
        scope_occurrences_keep_overrides_and_drop_with_the_event,
        scope_occurrences_reads_the_overlapping_window,
    },
    read::{
        account_scopes_enumerates_an_accounts_scopes, scope_objects_batch_reads_live_objects,
        structured_index_rows_replace_and_clear,
    },
};
