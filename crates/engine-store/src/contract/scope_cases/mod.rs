//! Scope-keyed contract cases: claim, apply (delta and snapshot), reconcile,
//! maintenance, and release.

mod apply;
mod lease;
mod read;

pub(super) use self::{
    apply::{
        container_and_member_scopes_are_independent, reconciliation_resolves_matching_op,
        reconciliation_skips_regressed_op, replay_is_idempotent, snapshot_tombstones_only_absent,
        streaming_page_keeps_cursor,
    },
    lease::{
        abandon_sync_leases_preserves_cursor_and_fences_old_worker, maintenance_is_lease_gated,
        release_with_stale_token_is_noop, scope_lease_is_exclusive_until_released,
        stale_lease_is_rejected,
    },
    read::{
        account_scopes_enumerates_an_accounts_scopes,
        scope_mail_index_reports_dates_threads_and_excludes_tombstones,
        scope_objects_batch_reads_live_objects, structured_index_rows_replace_and_clear,
    },
};
