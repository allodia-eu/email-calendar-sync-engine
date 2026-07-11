//! The calendar maintenance path: re-expanding stored events over a new horizon,
//! with no network.
//!
//! A sync only expands the objects its delta **changed** (`ScopeSyncer::derive`), which
//! is right for a sync and wrong for everything else. Two things move the occurrence
//! rows without any event changing:
//!
//! - a **horizon advance** — the host pages a calendar grid past what the last sync materialized;
//! - a **tzdata bump or display-zone change** — the same wall clock now resolves to a different
//!   instant, so every floating occurrence's `start_utc` is stale.
//!
//! In both cases the provider has nothing new to say: a CalDAV `sync-collection` reports
//! no changes, the delta is empty, and `derive` therefore expands nothing. Without this
//! path a host that widens its horizon and re-syncs gets an **empty grid, permanently**
//! — nothing short of a full reset would fill it.
//!
//! So this re-derives every *stored* event over the new horizon and commits the result
//! through `apply_maintenance` (`store-and-sync.md`). Each event's derived rows are
//! cleared and rewritten in one batch — the store applies `removed` before the upserts —
//! so changed instants replace stale ones atomically while unchanged ones stay
//! byte-stable.

use core::time::Duration;

use engine_core::{
    calendar::Event,
    ids::{AccountId, ProviderKey},
    search_index::{OwnerAddresses, project_event},
    sync::ObjectKind,
    time::{Horizon, TimeZoneId},
};
use engine_recurrence::expand;
use engine_store::{DerivedWrite, LeaseRequest, Store, StoreRead, WorkerId};

use crate::SyncError;

/// What a [`expand_calendar_horizon`] pass materialized, and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HorizonExpansion {
    /// How many occurrence rows the pass wrote across the account's calendars.
    pub occurrences: usize,
    /// The events the expander refused, with the reason.
    ///
    /// An unsupported recurrence rule or an unresolvable zone materializes **zero**
    /// occurrences, so the event is stored but renders nowhere on a grid — it does not
    /// merely look wrong, it is *absent*. Reported rather than discarded so a host can
    /// tell the user "this event can't be shown" instead of silently losing it.
    pub unexpandable: Vec<UnexpandableEvent>,
}

/// An event the expander could not materialize, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpandableEvent {
    /// The event's provider key.
    pub event: ProviderKey,
    /// The expander's rendered reason (an unsupported rule, or an unresolvable zone).
    pub reason: String,
}

/// Re-expands every stored event in `account`'s calendars over `horizon`, resolving
/// floating times through `host_zone`, and commits the fresh occurrences.
///
/// Network-free: it reads the events already in the store. Call it when the host widens
/// its horizon (before reading a window a sync never materialized), when the bundled
/// tzdata release changes, or when the user's display zone changes — a floating event's
/// stored instant is only correct for the zone it was expanded under.
///
/// It re-expands *every* event in the scope on every call, so a host should widen in
/// coarse chunks against a persisted watermark rather than call it per page.
///
/// # Errors
///
/// Returns [`SyncError`] if the store rejects a claim/apply, or a stored event payload
/// fails to deserialize. An event the *expander* rejects is **not** an error: it is
/// reported in [`HorizonExpansion::unexpandable`], so one unsupported rule cannot stop
/// the rest of the calendar from materializing.
pub async fn expand_calendar_horizon<S>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    horizon: Horizon,
    host_zone: &TimeZoneId,
) -> Result<HorizonExpansion, SyncError>
where
    S: Store + StoreRead,
{
    let req = LeaseRequest::new(worker, ttl);
    let scopes = store.account_scopes(account.clone()).await?;
    let mut report = HorizonExpansion::default();

    // One CalDAV account has one scope per calendar collection; a JMAP one has a single
    // event type. Enumerate rather than hard-code which (`SyncScope::object_kind`).
    for scope in scopes
        .into_iter()
        .filter(|scope| scope.object_kind() == Some(ObjectKind::Event))
    {
        let claim = store
            .claim_sync_scope(account.clone(), &scope, req.clone())
            .await?;
        let mut derived = DerivedWrite::empty();
        for key in store.object_keys(&scope).await? {
            let Some(payload) = store.object_payload(&scope, &key).await? else {
                continue;
            };
            let event: Event =
                serde_json::from_value(payload).map_err(|err| SyncError::decode(&err))?;
            // Clear this event's rows before rewriting them, so an instant that moved
            // (a horizon shrink, a tzdata bump) leaves no stale occurrence behind.
            derived.removed.push(key.clone());
            derived.push_event(project_event(&event, &OwnerAddresses::default()));
            match expand(&event, &horizon, host_zone) {
                Ok(occurrences) => derived.occurrences.extend(occurrences),
                Err(reason) => report.unexpandable.push(UnexpandableEvent {
                    event: key,
                    reason: reason.to_string(),
                }),
            }
        }
        report.occurrences += derived.occurrences.len();
        // Best-effort release on failure, so a held lease cannot block the next sync.
        if let Err(err) = store.apply_maintenance(&claim.lease, &derived).await {
            let _ = store.release_sync_scope(claim.lease).await;
            return Err(err.into());
        }
        store.release_sync_scope(claim.lease).await?;
    }
    Ok(report)
}
