//! The reusable store contract test suite.
//!
//! These are the invariants from `store-and-sync.md`, written once and run
//! against every [`Store`] + [`StoreRead`] backend (the in-memory reference store
//! here, `store-sqlite` and `store-postgres` later). Backends call [`run_all`]
//! from a `#[tokio::test]`.
//!
//! The cross-scope *apply ordering* invariant (containers before members) is an
//! orchestrator concern and is locked in `engine-sync`; this suite verifies the
//! store-level primitive it relies on — per-scope snapshot tombstoning is
//! isolated, and container and member scopes are independent units.
//!
//! Cases are split by surface — `scope_cases` (claim/apply/maintenance/release)
//! and `outbox_cases` (enqueue/claim/mark) — with the shared fixtures here.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    sync::{JmapDataType, Keyed, NoPatch, SyncObject, SyncScope},
    write::{IdempotencyKey, PendingOp, ResourceKey},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    lease::{LeaseRequest, ManualClock, WorkerId},
    store::{Store, StoreRead},
};

mod contact_cases;
mod outbox_cases;
mod scope_cases;

/// A trivial storable object the suite applies and reads back. Real domain types
/// (`Message`, `CalendarEvent`, …) implement [`Keyed`] the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TestObject {
    key: ProviderKey,
    data: String,
}

impl TestObject {
    fn new(key: &str, data: &str) -> Self {
        Self {
            key: pk(key),
            data: data.to_owned(),
        }
    }
}

impl Keyed for TestObject {
    fn provider_key(&self) -> &ProviderKey {
        &self.key
    }
}

impl SyncObject for TestObject {
    // The suite drives partial writes through `DerivedWrite` directly, which is where a store
    // receives them; nothing here needs a partial *update* form.
    type Patch = NoPatch;
}

fn acct(name: &str) -> AccountId {
    AccountId::try_from(name).expect("valid account id")
}

fn pk(value: &str) -> ProviderKey {
    ProviderKey::new(value).expect("valid provider key")
}

fn email_scope(account: &AccountId) -> SyncScope {
    SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Email,
    }
}

fn mailbox_scope(account: &AccountId) -> SyncScope {
    SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::Mailbox,
    }
}

fn event_scope(account: &AccountId) -> SyncScope {
    SyncScope::JmapType {
        account: account.clone(),
        data_type: JmapDataType::CalendarEvent,
    }
}

fn lease_request(owner: &str, ttl_secs: u64) -> LeaseRequest {
    LeaseRequest::new(WorkerId::new(owner), Duration::from_secs(ttl_secs))
}

fn pending_op(idempotency: &str, resource: &str) -> PendingOp {
    PendingOp::new(
        IdempotencyKey::new(idempotency).expect("valid idempotency key"),
        ResourceKey::new(resource).expect("valid resource key"),
        json!({ "idempotency": idempotency }),
    )
}

/// Runs the full store contract against a fresh store from `make` for each case.
///
/// `make` returns a store wired to a [`ManualClock`] the suite advances to drive
/// lease/TTL expiry. Every backend must pass this suite unchanged.
pub async fn run_all<S, F>(make: F)
where
    S: Store + StoreRead,
    F: Fn() -> (S, ManualClock),
{
    let (store, clock) = make();
    scope_cases::stale_lease_is_rejected(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::replay_is_idempotent(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::streaming_page_keeps_cursor(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::snapshot_tombstones_only_absent(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::reconciliation_skips_regressed_op(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::container_and_member_scopes_are_independent(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::scope_lease_is_exclusive_until_released(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::abandon_sync_leases_preserves_cursor_and_fences_old_worker(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::maintenance_is_lease_gated(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::reconciliation_resolves_matching_op(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::release_with_stale_token_is_noop(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::structured_index_rows_replace_and_clear(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::account_scopes_enumerates_an_accounts_scopes(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::schema_status_reports_a_store_this_build_can_use(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::scope_objects_batch_reads_live_objects(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::list_mail_orders_by_date_and_excludes_tombstones(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::list_mail_merges_accounts_into_one_order(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::list_mail_on_threads_gathers_only_the_named_threads(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::list_mail_by_keys_resolves_named_messages(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::a_resent_object_keeps_the_columns_no_provider_supplies(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::a_reply_joins_its_original_across_scopes(&store, &clock).await;
    scope_cases::a_smaller_owned_id_rekeys_every_member(&store, &clock).await;
    scope_cases::one_page_that_shares_an_id_is_one_thread(&store, &clock).await;
    scope_cases::a_provider_thread_is_never_joined_or_moved(&store, &clock).await;
    scope_cases::a_bare_message_threads_alone_and_stays(&store, &clock).await;
    scope_cases::ungrouped_graphed_mail_is_visible_to_the_store(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::state_change_moves_flags_and_leaves_the_rest(&store, &clock).await;
    scope_cases::state_change_carrying_filing_moves_the_message(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::state_change_for_an_unknown_message_writes_nothing(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::state_change_keeps_the_tokens_it_did_not_carry(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::scope_occurrences_reads_the_overlapping_window(&store, &clock).await;
    let (store, clock) = make();
    scope_cases::scope_occurrences_keep_overrides_and_drop_with_the_event(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::expired_op_lease_is_rejected(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::claim_filters_dependencies_and_resources(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::enqueue_is_idempotent(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::outcomes_record_failure_and_ambiguity(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::unknown_op_is_rejected_and_stateless(&store, &clock).await;
    let (store, clock) = make();
    outbox_cases::claim_respects_limit(&store, &clock).await;
}

/// Runs contact-generation, people-CAS, and recipient-history contracts.
///
/// Kept separate from [`run_all`] so a backend can implement the generic sync
/// contract before opting into the contact-derived store surface.
pub async fn run_contacts<S, F>(make: F)
where
    S: Store + StoreRead + crate::ContactStore,
    F: Fn() -> (S, ManualClock),
{
    let (store, _) = make();
    contact_cases::contact_generation_and_people_cas(&store).await;
    let (store, _) = make();
    contact_cases::recipient_idempotency_and_suppression(&store).await;
    let (store, _) = make();
    contact_cases::contact_photo_cache_is_fingerprint_bound(&store).await;
    contact_cases::contact_photo_cache_separates_resources_on_one_card(&store).await;
    let (store, clock) = make();
    contact_cases::a_recorded_absence_expires_and_is_revision_bound(&store, &clock).await;
    let (store, clock) = make();
    contact_cases::an_unrevisioned_photo_expires_and_a_revisioned_one_does_not(&store, &clock)
        .await;
    let (store, _) = make();
    contact_cases::people_resolve_from_a_batch_of_addresses(&store).await;
}
