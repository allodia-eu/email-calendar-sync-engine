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
    sync::{SyncScope, SyncState},
    time::{ExpansionWindow, TimeZoneId},
};
use engine_provider::{Provider, ProviderError, ScopeSync};
use engine_recurrence::{Horizon, expand};
use engine_store::{DerivedWrite, LeaseRequest, Store, StoreRead, SyncApplied, WorkerId};

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
/// A changed event is projected for search and **expanded into occurrences** before the
/// store commit. It expands over the window the **store** already holds
/// ([`ExpansionWindow`]) — not over `horizon`/`host_zone`, which only *seed* that window on
/// the very first sync of the scope, when there is nothing materialized yet. That is what
/// keeps a sync from silently narrowing what the host has expanded: a routine delta on a
/// one-week horizon must not delete a changed event's occurrences for the rest of the year
/// and leave every unchanged event's in place. Moving the window is
/// [`expand_calendar_horizon`](crate::expand_calendar_horizon)'s job, and it re-expands
/// *every* event to match.
///
/// An event whose recurrence is outside the expander's supported subset is still stored —
/// it just materializes no occurrences yet (`calendar-semantics.md`), never failing the
/// sync.
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
    S: Store + StoreRead,
{
    let req = LeaseRequest::new(worker, ttl);
    let (calendars, ()) = run_scope(store, account, &CalendarScope(provider), &req)
        .await?
        .into_applied();

    // The store's window wins. `horizon`/`host_zone` seed it only when the scope has never
    // been expanded, so a first sync still materializes something without the host having
    // to call `expand_calendar_horizon` first.
    let scope = provider.event_scope(account);
    let window = if let Some(window) = store.expansion_window(&scope).await? {
        window
    } else {
        let seeded = ExpansionWindow::new(horizon, host_zone.clone());
        seed_window(store, account, &scope, &req, &seeded).await?;
        seeded
    };
    let events = sync_event_scope(provider, store, account, &req, &window).await?;
    Ok(CalendarSyncReport { calendars, events })
}

/// Records the window a first sync will expand over, before it expands.
///
/// A short claim of its own: the scope row must exist (and be lease-gated) for the window
/// to be written, and `run_scope` takes and releases its own lease afterwards.
async fn seed_window<S>(
    store: &S,
    account: &AccountId,
    scope: &SyncScope,
    req: &LeaseRequest,
    window: &ExpansionWindow,
) -> Result<(), SyncError>
where
    S: Store,
{
    let claim = store
        .claim_sync_scope(account.clone(), scope, req.clone())
        .await?;
    let result = store.set_expansion_window(&claim.lease, window).await;
    store.release_sync_scope(claim.lease).await?;
    Ok(result?)
}

/// Re-reads one account's events through the provider's **delta** and commits the result:
/// the read-your-writes step a completed calendar write runs (see the module docs).
///
/// The event scope only — an event write cannot change the calendar *list*, so the
/// container scope is not fetched and not claimed.
///
/// One round trip on both transports **once the scope has a cursor**, which it does from its
/// first sync onward. With no cursor it is a full snapshot instead (CalDAV re-`REPORT`s every
/// resource with its `calendar-data` inline; JMAP queries and `/get`s every id) — correct,
/// but not cheap. In practice a host has synced the calendar before it can name one to write
/// to, and reconciling before *any* sync is refused outright
/// ([`SyncError::NoExpansionWindow`]).
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
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let scope = provider.event_scope(account);
    let window = store
        .expansion_window(&scope)
        .await?
        .ok_or(SyncError::NoExpansionWindow)?;
    let req = LeaseRequest::new(worker, ttl);
    sync_event_scope(provider, store, account, &req, &window).await
}

/// Runs the event scope once, collecting what the expander refused.
async fn sync_event_scope<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    window: &ExpansionWindow,
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let scope = EventScope {
        provider,
        window: window.clone(),
        unexpandable: Mutex::default(),
    };
    let (applied, ()) = run_scope(store, account, &scope, req).await?.into_applied();
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
    type Halt = core::convert::Infallible;
    type Meta = ();
    type Object = Calendar;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.calendar_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<crate::ScopeFetch<Calendar, (), Self::Halt>, ProviderError> {
        self.0
            .sync_calendars(account, cursor)
            .await
            .map(|sync| crate::ScopeFetch::Proceed { sync, meta: () })
    }

    fn derive(&self, _sync: &ScopeSync<Calendar>) -> DerivedWrite {
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
    /// The window the store holds — never one a caller passed. See [`sync_calendar`].
    window: ExpansionWindow,
    unexpandable: Mutex<Vec<UnexpandableEvent>>,
}

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EventScope<'_, P> {
    type Halt = core::convert::Infallible;
    type Meta = ();
    type Object = Event;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.provider.event_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<crate::ScopeFetch<Event, (), Self::Halt>, ProviderError> {
        self.provider
            .sync_events(account, cursor)
            .await
            .map(|sync| crate::ScopeFetch::Proceed { sync, meta: () })
    }

    fn derive(&self, sync: &ScopeSync<Event>) -> DerivedWrite {
        let mut derived = DerivedWrite::empty();
        let mut unexpandable = Vec::new();
        for event in changed_objects(&sync.update) {
            // Clear this event's occurrence rows before rewriting them. They are keyed by
            // `(scope, event, start, recurrence-id)` and *upserted*, so an event whose start
            // moved — or whose recurrence shrank — would otherwise keep its old rows beside
            // the new ones and render at both times. The store applies the reset before the
            // upserts in one transaction.
            //
            // The clear is unwindowed (every row for the event), which is only safe because
            // the re-expansion below covers the **store's** whole window rather than some
            // caller's horizon — otherwise it would delete occurrences outside that horizon
            // and never put them back.
            derived.reset_occurrences.push(event.id.key().clone());
            derived.push_event(project_event(event, &OwnerAddresses::default()));
            // An unsupported recurrence stores the event with no occurrences, never
            // failing the sync (`calendar-semantics.md`) — but it is then invisible to
            // every range read, so the reason travels out on the report rather than being
            // dropped here.
            match expand(event, &self.window.horizon, &self.window.zone) {
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
