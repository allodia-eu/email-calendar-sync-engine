//! The calendar sync path: the container and event scopes, the whole-calendar sync, and
//! the event-only delta a completed write reconciles through.
//!
//! # Why a write reconciles through the *read* primitive
//!
//! A calendar write returns a receipt, not a document: a CalDAV `PUT` answers with an
//! `ETag` and no body, a JMAP `CalendarEvent/set` with an id and no object. So after a
//! successful write the store still holds the **pre-write** event — its projection, its
//! `raw_ical`, and the revision the guard will be built from next time (issue #65).
//!
//! The tempting fix is to optimistically store the document we just sent, plus the
//! receipt's revision. It is wrong, and both halves of it are wrong:
//!
//! - **The bytes are not the server's.** Stalwart *reserializes* what it stores (it keeps every
//!   property but re-folds content lines and reorders `RRULE` parts) where SabreDAV stores them
//!   verbatim (`caldav.md`). Storing our own bytes would put a `RawIcal` in the store that the
//!   server does not have — and would **mask a server that silently dropped a property**, which is
//!   precisely the failure the round-trip tests exist to catch. The store's copy must keep coming
//!   from the server.
//! - **The revision cannot move without the body.** Writing only the new revision and leaving the
//!   body for the next sync is *worse than doing nothing*: the row would then claim a revision
//!   whose bytes we do not hold, so a host could patch the **stale body** under a **valid** guard,
//!   `PUT` it, and silently revert its own edit with a write the server happily accepts.
//!
//! So [`reconcile_calendar_events`] re-reads through the delta the sync path already uses.
//! It costs the same single round trip a refetch would, it hands back the server's
//! canonical copy (CalDAV's `sync-collection` carries `calendar-data` inline; JMAP's
//! `CalendarEvent/changes` back-references a `/get`), it tombstones a delete, and — unlike
//! a bare `GET` — it **advances the cursor**, so the change is not re-delivered on the next
//! pass. It needs no new provider verb.

use core::time::Duration;
use std::sync::Mutex;

use engine_core::{
    calendar::{Calendar, Event},
    ids::AccountId,
    search_index::{OwnerAddresses, project_event},
    sync::{SyncScope, SyncState, SyncUpdate},
    time::TimeZoneId,
};
use engine_provider::{Provider, ProviderError, ScopeSync};
use engine_recurrence::{Horizon, expand};
use engine_store::{DerivedWrite, LeaseRequest, Store, SyncApplied, WorkerId};

use crate::{ScopeSyncer, SyncError, UnexpandableEvent, changed_objects, run_scope};

/// What one `sync_calendar` run applied, per scope.
// Not `Copy`: `unexpandable` carries the events the expander refused, and losing that
// list to an implicit copy is exactly the silence this field exists to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarSyncReport {
    /// The calendar-container apply result.
    pub calendars: SyncApplied,
    /// The event-member apply result, and what it could not expand.
    pub events: EventSyncReport,
}

/// What one event-delta pass applied: the event half of [`sync_calendar`], and the whole
/// of [`reconcile_calendar_events`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventSyncReport {
    /// The event-member apply result.
    pub applied: SyncApplied,
    /// The synced events the expander could not materialize into occurrences.
    ///
    /// These are stored and appear in `Engine::events` — but they expand to **zero**
    /// occurrence rows, so they are invisible to every range read and render nowhere on a
    /// grid. Reported so a host can say "this event can't be shown" rather than lose it
    /// without a trace.
    pub unexpandable: Vec<UnexpandableEvent>,
}

/// Syncs one account's calendars: calendar containers first, then events.
///
/// Events are projected for search and **expanded into occurrences** over `horizon`
/// (resolving floating times through `host_zone`) before the store commit. An event whose
/// recurrence is outside the expander's supported subset is still stored — it just
/// materializes no occurrences yet (`calendar-semantics.md`), never failing the sync.
///
/// # Errors
///
/// Returns [`SyncError`] if the provider fetch fails or the store rejects the apply for a
/// reason other than a recoverable `StaleLease`.
pub async fn sync_calendar<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    horizon: Horizon,
    host_zone: &TimeZoneId,
) -> Result<CalendarSyncReport, SyncError>
where
    P: Provider,
    S: Store,
{
    let req = LeaseRequest::new(worker, ttl);
    let calendars = run_scope(store, account, &CalendarScope(provider), &req).await?;
    let events = sync_event_scope(provider, store, account, &req, horizon, host_zone).await?;
    Ok(CalendarSyncReport { calendars, events })
}

/// Re-reads one account's events through the provider's **delta** and commits the result:
/// the read-your-writes step a completed calendar write runs (see the module docs).
///
/// The event scope only — an event write cannot change the calendar *list*, so the
/// container scope is not fetched and not claimed. One round trip on both transports.
///
/// It is also the batch path: a host that writes many events runs one reconcile after the
/// last of them rather than one per write.
///
/// Occurrences are re-expanded over `horizon` through `host_zone`, exactly as a sync does
/// — so an edit that **moves** an event moves the rows a calendar grid reads, instead of
/// leaving the old instant beside the new one.
///
/// # Errors
///
/// Returns [`SyncError`] if the provider fetch fails or the store rejects the apply. A
/// caller reconciling a write it has already committed must **not** treat that as a failed
/// write: the write landed, and only the local copy is stale (see
/// `engine_api::Reconciled`).
pub async fn reconcile_calendar_events<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    horizon: Horizon,
    host_zone: &TimeZoneId,
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store,
{
    let req = LeaseRequest::new(worker, ttl);
    sync_event_scope(provider, store, account, &req, horizon, host_zone).await
}

/// Runs the event scope once, collecting what the expander refused.
async fn sync_event_scope<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    horizon: Horizon,
    host_zone: &TimeZoneId,
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store,
{
    let scope = EventScope {
        provider,
        horizon,
        host_zone: host_zone.clone(),
        unexpandable: Mutex::default(),
    };
    let applied = run_scope(store, account, &scope, req).await?;
    Ok(EventSyncReport {
        applied,
        unexpandable: scope
            .unexpandable
            .into_inner()
            .expect("unexpandable mutex poisoned"),
    })
}

/// The calendar-container scope syncer.
struct CalendarScope<'p, P>(&'p P);

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for CalendarScope<'_, P> {
    type Object = Calendar;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.calendar_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeSync<Calendar>, ProviderError> {
        self.0.sync_calendars(account, cursor).await
    }

    fn derive(&self, _update: &SyncUpdate<Calendar>) -> DerivedWrite {
        DerivedWrite::empty()
    }
}

/// The event-member scope syncer: projects each event and expands its occurrences over
/// the horizon.
///
/// `derive` cannot fail (its signature returns the rows, not a `Result`), but an event the
/// expander refuses materializes **no occurrences** — it is stored, yet renders nowhere on
/// a grid. Collecting those here rather than discarding them lets the caller report them,
/// so the absence is visible instead of silent.
struct EventScope<'p, P> {
    provider: &'p P,
    horizon: Horizon,
    host_zone: TimeZoneId,
    unexpandable: Mutex<Vec<UnexpandableEvent>>,
}

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EventScope<'_, P> {
    type Object = Event;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.provider.event_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeSync<Event>, ProviderError> {
        self.provider.sync_events(account, cursor).await
    }

    fn derive(&self, update: &SyncUpdate<Event>) -> DerivedWrite {
        let mut derived = DerivedWrite::empty();
        let mut unexpandable = Vec::new();
        for event in changed_objects(update) {
            // Clear the event's derived rows before rewriting them. Occurrences are keyed
            // by `(scope, event, start, recurrence-id)` and *upserted*, so an event whose
            // start moved — or whose recurrence shrank — would otherwise keep its old
            // occurrence rows beside the new ones and render at both times. Every other
            // derived row is re-inserted from this same projection, and the store applies
            // `removed` before the upserts in one transaction (`horizon.rs` does the same
            // for a re-expansion).
            derived.removed.push(event.id.key().clone());
            derived.push_event(project_event(event, &OwnerAddresses::default()));
            // An unsupported recurrence stores the event with no occurrences, never
            // failing the sync (`calendar-semantics.md`) — but it is then invisible to
            // every range read, so the reason travels out on the report rather than being
            // dropped here.
            match expand(event, &self.horizon, &self.host_zone) {
                Ok(occurrences) => derived.occurrences.extend(occurrences),
                Err(reason) => unexpandable.push(UnexpandableEvent {
                    event: event.id.key().clone(),
                    reason: reason.to_string(),
                }),
            }
        }
        // `run_scope` re-derives on a stale-lease reclaim, so replace rather than append:
        // a retry must not report the same event twice.
        *self
            .unexpandable
            .lock()
            .expect("unexpandable mutex poisoned") = unexpandable;
        derived
    }
}
