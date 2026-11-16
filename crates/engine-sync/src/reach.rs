//! Fetching a further range into an already synced calendar.
//!
//! Most transports fetch a calendar whole: a CalDAV `sync-collection` or a JMAP
//! `CalendarEvent/changes` reports every event the calendar has, whenever it is. Graph and Google
//! do not ([`Capabilities::windowed_events`](engine_provider::Capabilities::windowed_events)):
//! a snapshot lists only the events inside the date window it was asked for, and the cursor it
//! ends in continues from there. Outside that window the store holds what happened to arrive (a
//! recurring series whose master lies inside, a change Google's feed reported) and lacks every
//! one-off event nobody touched since. A host paging its grid past the window would draw those
//! days confidently short.
//!
//! # Reopening, not appending
//!
//! The obvious fix, listing the new range once and merging it in, leaves those events stale: a
//! Graph `deltaLink` reports changes only inside the window it was opened over (measured,
//! `provider-graph/tests/live_calendar_window.rs`), and Google does not document what a
//! `syncToken` taken after a bounded list reports outside its bounds. A range fetched beside
//! the cursor would show the event as it was on the day it was fetched, forever.
//!
//! So [`reach_calendar`] **reopens** the scope's cursor over the union of the window it was
//! opened over and the range asked for, as a fresh snapshot. One cursor then covers every event
//! the store claims to hold, on every transport, whatever its feed does past its bounds. It
//! costs a snapshot of the whole union rather than of the new days, which is why a host asks
//! for ranges in coarse steps.
//!
//! # Coverage
//!
//! The window a cursor was opened over is recorded with it
//! ([`ApplyBatch::fetch_window`](engine_store::ApplyBatch::fetch_window)), in the same commit, so
//! the two cannot disagree. It only grows: a reopen covers the union, and an ordinary delta
//! leaves it. A range inside it is answered without a request, and a range is never fetched
//! twice: a second caller asking for the same range waits for the first one's lease, then finds
//! it covered.
//!
//! # While it runs, and when it fails
//!
//! The reopen holds the scope's lease for its fetch. Reads go on answering from the store as it
//! was, so the host keeps drawing what it had until the call returns and the new events land in
//! one commit. A sync of the same calendar meanwhile finds the scope held and reports it so; a
//! re-expansion waits ([`expand_calendar_horizon`](crate::expand_calendar_horizon)).
//!
//! A scope that fails is reported on its own ([`EventScopeReach::result`]) and records nothing:
//! its old cursor and window stand, and asking again fetches the range again.

use core::time::Duration;
use std::collections::BTreeMap;

use engine_core::{
    ids::{AccountId, CalendarId},
    sync::SyncScope,
    time::Horizon,
};
use engine_provider::Provider;
use engine_store::{LeaseRequest, Store, StoreRead, WorkerId};
use futures_util::{StreamExt, stream};

use crate::{
    EventSyncReport, SyncError,
    calendar::{
        MAX_CONCURRENT_EVENT_SCOPES, ScopeGroup, event_scopes, stored_calendars, sync_event_scope,
    },
    calendar_scopes::{EventFetch, EventScope},
    scope_wait::while_held,
};

/// What one [`reach_calendar`] call did, per event scope of the account.
#[derive(Debug)]
pub struct CalendarReach {
    /// Each event scope's outcome.
    pub scopes: Vec<EventScopeReach>,
}

impl CalendarReach {
    /// The event scopes that failed, with why. Until they succeed, the store does not hold the
    /// whole range for their calendars.
    pub fn failures(&self) -> impl Iterator<Item = (&SyncScope, &SyncError)> {
        self.scopes
            .iter()
            .filter_map(|scope| scope.result.as_ref().err().map(|err| (&scope.scope, err)))
    }

    /// Whether every event scope now holds every event of the range.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failures().next().is_none()
    }
}

/// One event scope's outcome in a [`CalendarReach`].
#[derive(Debug)]
pub struct EventScopeReach {
    /// The scope.
    pub scope: SyncScope,
    /// The calendars whose events it holds.
    pub calendars: Vec<CalendarId>,
    /// What the call did with it.
    pub result: Result<ScopeReach, SyncError>,
}

/// What [`reach_calendar`] did with one event scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeReach {
    /// The store already held every event of the range, so nothing was asked of the server: the
    /// transport fetches the calendar whole, or the scope's cursor was opened over a window that
    /// contains the range.
    Held,
    /// The scope was fetched and applied: its cursor reopened over the union of the window it
    /// had and the range, or its first fetch where it had none.
    Fetched(EventSyncReport),
}

/// Makes the store hold every event of `account`'s calendars that falls in `range`, and keeps
/// them current from then on (see the module docs).
///
/// Each event scope is decided on its own, concurrently: one whose cursor already covers `range`
/// is left alone, and one whose transport fetches calendars whole is too, once it has synced. A
/// scope another pass holds is waited for rather than reported busy, since that pass would not
/// fetch the range. Fetched events are expanded over the window the store holds, as a sync
/// expands them, so a host widening what it **shows** still calls
/// [`expand_calendar_horizon`](crate::expand_calendar_horizon) for that; the two are
/// independent, and either order works.
///
/// # Errors
///
/// Returns [`SyncError`] if the stored calendar list cannot be read. Everything after that is
/// reported per scope, a scope no [`sync_calendar`](crate::sync_calendar) has reached as
/// [`SyncError::NoExpansionWindow`].
pub async fn reach_calendar<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    range: Horizon,
) -> Result<CalendarReach, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let req = LeaseRequest::new(worker, ttl);
    let listed = stored_calendars(provider, store, account).await?;
    let groups = event_scopes(provider, account, &listed, &BTreeMap::new());
    let scopes = stream::iter(groups.into_iter().map(|group| {
        let req = &req;
        async move {
            let result =
                while_held(|| reach_scope(provider, store, account, req, &group, range)).await;
            EventScopeReach {
                scope: group.scope,
                calendars: group.calendars.into_iter().map(|c| c.id).collect(),
                result,
            }
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_EVENT_SCOPES)
    .collect()
    .await;
    Ok(CalendarReach { scopes })
}

/// Decides and, where the store lacks the range, fetches one scope.
///
/// Fails [`SyncError::is_busy`] having done nothing when another pass holds the scope; every
/// read it decides on is made again on the next try.
async fn reach_scope<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    req: &LeaseRequest,
    group: &ScopeGroup,
    range: Horizon,
) -> Result<ScopeReach, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let expansion = store
        .expansion_window(&group.scope)
        .await?
        .ok_or(SyncError::NoExpansionWindow)?;
    let windowed = provider.connection_info().capabilities.windowed_events();
    let recorded = store.fetch_window(&group.scope).await?;
    let synced = store
        .load_sync_state(account.clone(), &group.scope)
        .await?
        .is_some();
    let covered = !windowed || recorded.is_some_and(|window| window.contains(range));
    if synced && covered {
        return Ok(ScopeReach::Held);
    }
    // A scope with a cursor but no recorded window was opened before windows were recorded,
    // over one the host chose then. The range the host shows is the best stand-in for it.
    let window = recorded.unwrap_or(expansion.horizon).union(range);
    let fetch = if windowed {
        EventFetch::Within {
            window,
            reopen: true,
        }
    } else {
        EventFetch::Whole
    };
    let calendar = group.calendars[0].id.clone();
    let scope = EventScope::new(provider, calendar, expansion, None, fetch);
    let report = sync_event_scope(store, account, req, scope).await?;
    Ok(ScopeReach::Fetched(report))
}
