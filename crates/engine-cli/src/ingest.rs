//! The ingest and horizon-maintenance pipeline.
//!
//! Ingest is the project → expand → apply path the store contract prescribes
//! (`store-and-sync.md`): pure engine code computes the derived rows (text/
//! structured projections and expanded occurrences) *before* the atomic store
//! call. [`reexpand_calendar`] is the maintenance path for the two non-sync
//! triggers (`store-and-sync.md`): a **horizon advance** (materialize further out)
//! and a **tzdata-version bump** (re-expand under the new release). Both re-derive
//! each event in the scope over the given horizon and commit through
//! `apply_maintenance` under the scope lease; the per-scope fan-out a real engine
//! drives from sync state is the orchestrator's job (a later step).

use core::time::Duration;

use engine_core::calendar::Event;
use engine_core::ids::AccountId;
use engine_core::mail::Message;
use engine_core::search_index::{OwnerAddresses, project_event, project_message};
use engine_core::sync::{SyncState, SyncUpdate};
use engine_core::time::TimeZoneId;
use engine_recurrence::{Horizon, expand};
use engine_store::{ApplyBatch, Clock, DerivedWrite, LeaseRequest, Store, StoreRead, WorkerId};
use store_sqlite::SqliteStore;

use crate::{CURSOR, CliError, Fixture, WORKER, calendar_scope, mail_scope};

/// How long each harness lease is held; the fixed clock keeps it live for the run.
const LEASE_TTL: Duration = Duration::from_mins(5);

/// Counts from an ingest, for reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IngestReport {
    /// Mail messages applied.
    pub messages: usize,
    /// Calendar events applied.
    pub events: usize,
    /// Occurrences materialized from those events within the horizon.
    pub occurrences: usize,
}

/// The harness lease request.
fn lease() -> LeaseRequest {
    LeaseRequest::new(WorkerId::new(WORKER), LEASE_TTL)
}

/// Ingests a fixture: mail under the account's mail scope, events under its
/// calendar scope (projected and expanded into occurrences).
///
/// # Errors
///
/// Returns [`CliError`] if a store operation, projection, or expansion fails.
pub async fn ingest<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    fixture: &Fixture,
    horizon: &Horizon,
    host_zone: &TimeZoneId,
) -> Result<IngestReport, CliError> {
    let mut report = IngestReport::default();
    if !fixture.messages.is_empty() {
        report.messages = ingest_mail(store, account.clone(), &fixture.messages).await?;
    }
    if !fixture.events.is_empty() {
        let (events, occurrences) =
            ingest_calendar(store, account, &fixture.events, horizon, host_zone).await?;
        report.events = events;
        report.occurrences = occurrences;
    }
    Ok(report)
}

/// Applies mail messages with their text/structured projections.
async fn ingest_mail<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    messages: &[Message],
) -> Result<usize, CliError> {
    let scope = mail_scope(account.clone());
    let claim = store.claim_sync_scope(account, &scope, lease()).await?;
    let mut derived = DerivedWrite::empty();
    for message in messages {
        derived.push_mail(project_message(message));
    }
    let update = SyncUpdate::delta(messages.to_vec(), Vec::new());
    let next = SyncState::new(CURSOR);
    store
        .apply_sync_update(&claim.lease, ApplyBatch::new(&update, &derived, &[], &next))
        .await?;
    store.release_sync_scope(claim.lease).await?;
    Ok(messages.len())
}

/// Applies calendar events with their projections and expanded occurrences.
async fn ingest_calendar<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    events: &[Event],
    horizon: &Horizon,
    host_zone: &TimeZoneId,
) -> Result<(usize, usize), CliError> {
    let scope = calendar_scope(account.clone());
    let claim = store.claim_sync_scope(account, &scope, lease()).await?;
    let mut derived = DerivedWrite::empty();
    for event in events {
        derived.push_event(project_event(event, &OwnerAddresses::default()));
        derived
            .occurrences
            .extend(expand(event, horizon, host_zone)?);
    }
    let occurrences = derived.occurrences.len();
    let update = SyncUpdate::delta(events.to_vec(), Vec::new());
    let next = SyncState::new(CURSOR);
    store
        .apply_sync_update(&claim.lease, ApplyBatch::new(&update, &derived, &[], &next))
        .await?;
    store.release_sync_scope(claim.lease).await?;
    Ok((events.len(), occurrences))
}

/// Re-expands every event in the calendar scope over `horizon` and commits the
/// fresh occurrences through `apply_maintenance` — the maintenance path for a
/// horizon advance or a tzdata-version bump.
///
/// Each event's derived rows are cleared and rewritten in one batch (the store
/// applies `removed` before the upserts), so changed occurrence instants replace
/// stale ones atomically while unchanged ones stay byte-stable.
///
/// # Errors
///
/// Returns [`CliError`] if a store operation, a stored payload's deserialization,
/// or an expansion fails.
pub async fn reexpand_calendar<C: Clock>(
    store: &SqliteStore<C>,
    account: AccountId,
    horizon: &Horizon,
    host_zone: &TimeZoneId,
) -> Result<usize, CliError> {
    let scope = calendar_scope(account.clone());
    let claim = store.claim_sync_scope(account, &scope, lease()).await?;
    let keys = store.object_keys(&scope).await?;
    let mut derived = DerivedWrite::empty();
    for key in &keys {
        let Some(payload) = store.object_payload(&scope, key).await? else {
            continue;
        };
        let event: Event =
            serde_json::from_value(payload).map_err(|e| CliError::Fixture(e.to_string()))?;
        derived.removed.push(key.clone());
        derived.push_event(project_event(&event, &OwnerAddresses::default()));
        derived
            .occurrences
            .extend(expand(&event, horizon, host_zone)?);
    }
    let occurrences = derived.occurrences.len();
    store.apply_maintenance(&claim.lease, &derived).await?;
    store.release_sync_scope(claim.lease).await?;
    Ok(occurrences)
}
