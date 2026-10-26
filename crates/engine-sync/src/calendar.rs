//! The calendar sync path: the account's calendar list, every calendar's events, and the
//! event-only delta a completed write reconciles through.
//!
//! # One provider, every calendar
//!
//! A provider serves every calendar of the account it signed in to, and [`sync_calendar`]
//! owns the fan-out: it reads the calendar list once, groups the calendars by the event scope
//! each answers ([`Provider::event_scope`]), and syncs those scopes concurrently. Where a
//! protocol's event delta is per calendar (CalDAV, Graph, Google) that is one scope per
//! calendar; where it is account-wide (JMAP) every calendar shares one, synced once.
//!
//! A calendar whose listing carries a [`CollectionTag`] equal to the one its scope recorded at
//! its last sync has not changed, and is skipped without a request. On a CalDAV account that
//! makes a quiet refresh one `PROPFIND` of the calendar home, however many calendars it has.
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
use std::collections::BTreeSet;

use engine_core::{
    calendar::Calendar,
    ids::{AccountId, CalendarId},
    sync::{ObjectKind, SyncScope},
    time::{ExpansionWindow, TimeZoneId},
    version::CollectionTag,
};
use engine_provider::Provider;
use engine_recurrence::Horizon;
use engine_store::{LeaseRequest, Store, StoreRead, SyncApplied, WorkerId};
use futures_util::{StreamExt, stream};

use crate::{
    SyncError, UnexpandableEvent,
    calendar_scopes::{CalendarScope, EventScope},
    run_scope, vanished,
};

/// How many of an account's event scopes sync at once.
///
/// A store figure, like [`sync_mail`](crate::sync_mail)'s folder bound: what it limits is how
/// many scopes contend for the store's single write connection. The requests themselves share
/// one HTTP client per provider, so they reuse its connections whatever this is.
const MAX_CONCURRENT_EVENT_SCOPES: usize = 4;

/// What one [`sync_calendar`] run did: the calendar list, then each event scope.
#[derive(Debug)]
pub struct CalendarSyncReport {
    /// The calendar-container apply result.
    pub calendars: SyncApplied,
    /// The event scopes that synced, summed: every scope's applied counts and everything the
    /// expander refused. A scope that failed or was unchanged adds nothing; see
    /// [`scopes`](Self::scopes) and [`failures`](Self::failures).
    pub events: EventSyncReport,
    /// Each event scope's outcome, one per scope the account's calendars answer.
    pub scopes: Vec<EventScopeSync>,
    /// How many event scopes were dropped because no listed calendar answers them any more.
    pub forgotten: usize,
}

impl CalendarSyncReport {
    /// The event scopes that failed, with why. A pass that reports any is incomplete: the
    /// other calendars synced, these did not.
    pub fn failures(&self) -> impl Iterator<Item = (&SyncScope, &SyncError)> {
        self.scopes
            .iter()
            .filter_map(|scope| scope.result.as_ref().err().map(|err| (&scope.scope, err)))
    }

    /// How many event scopes were skipped because their calendar had not changed.
    #[must_use]
    pub fn unchanged(&self) -> usize {
        self.scopes
            .iter()
            .filter(|scope| matches!(scope.result, Ok(EventScopePass::Unchanged)))
            .count()
    }
}

/// One event scope's outcome in a [`CalendarSyncReport`].
#[derive(Debug)]
pub struct EventScopeSync {
    /// The scope.
    pub scope: SyncScope,
    /// The calendars whose events it holds: one, except where a protocol's event delta is
    /// account-wide.
    pub calendars: Vec<CalendarId>,
    /// What the pass did with it. A scope another worker holds is
    /// [`StoreError::ScopeHeld`](engine_store::StoreError::ScopeHeld) ([`SyncError::is_busy`]):
    /// that worker brings the same changes.
    pub result: Result<EventScopePass, SyncError>,
}

/// What a pass did with one event scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScopePass {
    /// The calendar's listing tag equals the one recorded at its last sync, so nothing was
    /// asked of it.
    Unchanged,
    /// The scope was fetched and applied.
    Synced(EventSyncReport),
}

/// What one event-delta pass applied: one scope's half of [`sync_calendar`], and the whole of
/// [`reconcile_calendar_events`].
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

impl EventSyncReport {
    /// Adds `other`'s counts and refusals to this report: the sum of several scopes' passes.
    pub fn absorb(&mut self, other: &Self) {
        self.applied.upserted += other.applied.upserted;
        self.applied.tombstoned += other.applied.tombstoned;
        self.applied.reconciled += other.applied.reconciled;
        self.unexpandable.extend(other.unexpandable.iter().cloned());
    }
}

/// Syncs one account's calendars: the calendar list first, then every calendar's events.
///
/// The event scopes sync concurrently, each under its own lease, and each reports its own
/// outcome: a calendar the server refuses (a share withdrawn, a feed gone) fails alone, and
/// the others still sync. An event scope no listed calendar answers any more is forgotten
/// whole, as a vanished mail folder is.
///
/// A changed event is projected for search and **expanded into occurrences** before the
/// store commit. It expands over the window the **store** already holds
/// ([`ExpansionWindow`]) — not over `horizon`/`host_zone`, which only *seed* that window on
/// the very first sync of the account, when there is nothing materialized yet. That is what
/// keeps a sync from silently narrowing what the host has expanded: a routine delta on a
/// one-week horizon must not delete a changed event's occurrences for the rest of the year
/// and leave every unchanged event's in place. A calendar that appears later starts from the
/// window the account's other calendars already hold. Moving the window is
/// [`expand_calendar_horizon`](crate::expand_calendar_horizon)'s job, and it re-expands
/// *every* event to match.
///
/// An event whose recurrence is outside the expander's supported subset is still stored —
/// it just materializes no occurrences yet (`calendar-semantics.md`), never failing the
/// sync.
///
/// # Errors
///
/// Returns [`SyncError`] if the calendar list cannot be fetched or applied, or the stored
/// list cannot be read back. Everything after that is reported per scope.
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
    let listed = stored_calendars(provider, store, account).await?;
    let groups = event_scopes(provider, account, &listed);

    // Before the fan-out, so a calendar that has just left the list is not synced once more.
    // An empty list judges nothing: it is far more likely a list that failed to arrive.
    let forgotten = if listed.is_empty() {
        0
    } else {
        let keep: BTreeSet<&SyncScope> = groups.iter().map(|group| &group.scope).collect();
        let stale = store
            .account_scopes(account.clone())
            .await?
            .into_iter()
            .filter(|scope| scope.object_kind() == Some(ObjectKind::Event) && !keep.contains(scope))
            .collect::<Vec<_>>();
        vanished::forget_scopes(store, account, &req, &stale).await?
    };

    let seed = account_window(store, &groups)
        .await?
        .unwrap_or_else(|| ExpansionWindow::new(horizon, host_zone.clone()));
    let scopes: Vec<EventScopeSync> = stream::iter(groups.into_iter().map(|group| {
        let (req, seed) = (&req, &seed);
        async move {
            let result = sync_group(provider, store, account, req, seed, &group).await;
            EventScopeSync {
                scope: group.scope,
                calendars: group.calendars.into_iter().map(|c| c.id).collect(),
                result,
            }
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_EVENT_SCOPES)
    .collect()
    .await;

    let mut events = EventSyncReport::default();
    for scope in &scopes {
        if let Ok(EventScopePass::Synced(report)) = &scope.result {
            events.absorb(report);
        }
    }
    Ok(CalendarSyncReport {
        calendars,
        events,
        scopes,
        forgotten,
    })
}

/// The calendars one event scope holds.
struct ScopeGroup {
    scope: SyncScope,
    calendars: Vec<Calendar>,
}

impl ScopeGroup {
    /// The tag the scope is gated on: its calendar's, when it holds exactly one. A scope
    /// several calendars share has no single listing tag to compare.
    fn tag(&self) -> Option<&CollectionTag> {
        match self.calendars.as_slice() {
            [only] => only.collection_tag.as_ref(),
            _ => None,
        }
    }
}

/// The account's calendars as the list scope now stores them, the default one first.
async fn stored_calendars<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
) -> Result<Vec<Calendar>, SyncError>
where
    P: Provider,
    S: StoreRead,
{
    let mut calendars = Vec::new();
    for (_key, payload) in store
        .scope_objects(&provider.calendar_scope(account))
        .await?
    {
        calendars
            .push(serde_json::from_value::<Calendar>(payload).map_err(|e| SyncError::decode(&e))?);
    }
    // Stable, so the rest keep the provider's sort hint and then the store's order.
    calendars.sort_by_key(|calendar| (!calendar.is_default, calendar.sort_order));
    Ok(calendars)
}

/// Groups `calendars` by the event scope each answers, in the order the first of each appears.
fn event_scopes<P: Provider>(
    provider: &P,
    account: &AccountId,
    calendars: &[Calendar],
) -> Vec<ScopeGroup> {
    let mut groups: Vec<ScopeGroup> = Vec::new();
    for calendar in calendars {
        let scope = provider.event_scope(account, &calendar.id);
        match groups.iter_mut().find(|group| group.scope == scope) {
            Some(group) => group.calendars.push(calendar.clone()),
            None => groups.push(ScopeGroup {
                scope,
                calendars: vec![calendar.clone()],
            }),
        }
    }
    groups
}

/// The window the account's event scopes are already expanded over, if any is.
async fn account_window<S: StoreRead>(
    store: &S,
    groups: &[ScopeGroup],
) -> Result<Option<ExpansionWindow>, SyncError> {
    for group in groups {
        if let Some(window) = store.expansion_window(&group.scope).await? {
            return Ok(Some(window));
        }
    }
    Ok(None)
}

/// Syncs one event scope, or skips it when its calendar's tag has not moved.
async fn sync_group<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    seed: &ExpansionWindow,
    group: &ScopeGroup,
) -> Result<EventScopePass, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let tag = group.tag();
    // A lease-free read: a tag is only ever recorded with a cursor, so a match means a pass
    // has already applied everything up to this state of the calendar.
    if tag.is_some() && store.collection_tag(&group.scope).await?.as_ref() == tag {
        return Ok(EventScopePass::Unchanged);
    }
    let window = if let Some(window) = store.expansion_window(&group.scope).await? {
        window
    } else {
        seed_window(store, account, &group.scope, req, seed).await?;
        seed.clone()
    };
    let calendar = group.calendars[0].id.clone();
    let report = sync_event_scope(
        provider,
        store,
        account,
        req,
        calendar,
        window,
        tag.cloned(),
    )
    .await?;
    Ok(EventScopePass::Synced(report))
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

/// Re-reads the events of `calendar`'s scope through the provider's **delta** and commits the
/// result: the read-your-writes step a completed calendar write runs (see the module docs).
///
/// The event scope only — an event write cannot change the calendar *list*, so the
/// container scope is not fetched and not claimed — and only the scope of the calendar the
/// write touched, so a write to one calendar costs one delta however many the account has.
///
/// One round trip on every transport **once the scope has a cursor**, which it does from its
/// first sync onward. With no cursor it is a full snapshot instead (CalDAV re-`REPORT`s every
/// resource with its `calendar-data` inline; JMAP queries and `/get`s every id) — correct,
/// but not cheap. In practice a host has synced the calendar before it can name one to write
/// to, and reconciling a calendar no pass has reached is refused outright
/// ([`SyncError::NoExpansionWindow`]).
///
/// It records no collection tag, which clears the scope's: the write moved the calendar's
/// tag, and the next pass reads the new one with the listing.
///
/// Occurrences are re-expanded over the window the store holds, exactly as a sync does —
/// so an edit that **moves** an event moves the rows a calendar grid reads, instead of
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
    calendar: &CalendarId,
    worker: WorkerId,
    ttl: Duration,
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let scope = provider.event_scope(account, calendar);
    let window = store
        .expansion_window(&scope)
        .await?
        .ok_or(SyncError::NoExpansionWindow)?;
    let req = LeaseRequest::new(worker, ttl);
    sync_event_scope(
        provider,
        store,
        account,
        &req,
        calendar.clone(),
        window,
        None,
    )
    .await
}

/// Runs one event scope once, collecting what the expander refused.
async fn sync_event_scope<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    calendar: CalendarId,
    window: ExpansionWindow,
    tag: Option<CollectionTag>,
) -> Result<EventSyncReport, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let scope = EventScope::new(provider, calendar, window, tag);
    let (applied, ()) = run_scope(store, account, &scope, req).await?.into_applied();
    Ok(EventSyncReport {
        applied,
        unexpandable: scope.into_unexpandable(),
    })
}
