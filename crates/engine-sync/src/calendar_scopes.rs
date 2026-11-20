//! The two calendar scope syncers [`run_scope`](crate::run_scope) drives: the account's
//! calendar list, and the events of one event scope.

use std::{collections::BTreeMap, sync::Mutex};

use engine_core::{
    calendar::{Calendar, Event},
    ids::{AccountId, CalendarId},
    search_index::{OwnerAddresses, project_event},
    sync::{SyncScope, SyncState},
    time::{ExpansionWindow, Horizon},
    version::CollectionTag,
};
use engine_provider::{Provider, ProviderError, ScopeSync};
use engine_recurrence::expand;
use engine_store::{DerivedWrite, StoreRead};

use crate::{ScopeFetch, ScopeSyncer, SyncError, UnexpandableEvent, changed_objects};

/// The calendar-container scope syncer. Its meta is the tag the listing showed for each
/// calendar, which the store keeps with that calendar's event cursor rather than on its row.
pub(crate) struct CalendarScope<'p, P>(pub(crate) &'p P);

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for CalendarScope<'_, P> {
    type Halt = core::convert::Infallible;
    type Meta = BTreeMap<CalendarId, CollectionTag>;
    type Object = Calendar;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.0.calendar_scope(account)
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Calendar, Self::Meta, Self::Halt>, ProviderError> {
        self.0
            .sync_calendars(account, cursor)
            .await
            .map(|listing| ScopeFetch::Proceed {
                sync: listing.sync,
                meta: listing.tags,
            })
    }

    fn derive(&self, _sync: &ScopeSync<Calendar>) -> DerivedWrite {
        DerivedWrite::empty()
    }
}

/// The event-member scope syncer: fetches one event scope through `calendar`, projects each
/// event and expands its occurrences over the store's window.
///
/// What it fetches and expands over is read from the store **under the scope's lease**
/// ([`ScopeSyncer::prepare`]), never before the claim: the pass that held the lease may have
/// widened the fetch window or the expansion window meanwhile, and a fetch planned on the old
/// ones would undo it (a reopen over a smaller union deletes the events that pass reached, and
/// changed events expanded over the old horizon leave the new days empty).
///
/// `derive` cannot fail (its signature returns the rows, not a `Result`), but an event the
/// expander refuses materializes **no occurrences**: it is stored, yet renders nowhere on a
/// grid. Collecting those here rather than discarding them lets the caller report them, so the
/// absence is visible instead of silent.
pub(crate) struct EventScope<'p, P> {
    pub(crate) provider: &'p P,
    /// The calendar the scope is fetched through; any calendar of a shared scope answers for
    /// all of them.
    pub(crate) calendar: CalendarId,
    /// What the pass is for.
    plan: EventPlan,
    /// The listing's tag for the calendar, read before the fetch, recorded with the cursor.
    tag: Option<CollectionTag>,
    /// What [`prepare`](ScopeSyncer::prepare) read under the lease.
    resolved: Mutex<Option<Resolved>>,
    pub(crate) unexpandable: Mutex<Vec<UnexpandableEvent>>,
}

/// What an [`EventScope`] pass is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventPlan {
    /// Follow the scope's cursor inside the window it was opened over: a sync, or the
    /// reconcile after a write.
    Follow,
    /// Make the store hold every event in the range: reopen the cursor over the union of its
    /// window and the range, unless the scope already holds it
    /// ([`reach_calendar`](crate::reach_calendar)).
    Reach(Horizon),
}

/// A [`EventPlan::Reach`] pass that found the range already held: no request was made.
#[derive(Debug)]
pub(crate) struct Covered;

/// The windows a pass runs over, read under its lease.
#[derive(Debug, Clone)]
struct Resolved {
    /// The window the store expands over. See [`sync_calendar`](crate::sync_calendar).
    expansion: ExpansionWindow,
    fetch: EventFetch,
    /// The tag recorded with the cursor: the listing's for a sync, the one the scope already
    /// had for a reach.
    tag: Option<CollectionTag>,
}

/// How an [`EventScope`] asks its provider for events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EventFetch {
    /// The window the provider opens any cursor it must open over
    /// ([`CalendarSync::sync_events`]); a transport that fetches a calendar whole ignores it.
    window: Horizon,
    /// Whether the transport fetches inside a window
    /// ([`windowed_events`](engine_provider::Capabilities::windowed_events)), so the window a
    /// snapshot opened over is recorded with its cursor.
    windowed: bool,
    /// Open a fresh cursor over `window` even where the scope has one: how a range past the
    /// recorded window is fetched.
    reopen: bool,
}

impl<'p, P> EventScope<'p, P> {
    pub(crate) fn new(
        provider: &'p P,
        calendar: CalendarId,
        plan: EventPlan,
        tag: Option<CollectionTag>,
    ) -> Self {
        Self {
            provider,
            calendar,
            plan,
            tag,
            resolved: Mutex::default(),
            unexpandable: Mutex::default(),
        }
    }

    pub(crate) fn into_unexpandable(self) -> Vec<UnexpandableEvent> {
        self.unexpandable
            .into_inner()
            .expect("unexpandable mutex poisoned")
    }

    fn resolved(&self) -> Resolved {
        self.resolved
            .lock()
            .expect("resolved mutex poisoned")
            .clone()
            .expect("prepare runs before the fetch")
    }
}

impl<P: Provider> EventScope<'_, P> {
    /// Plans the pass from what the store holds now, under the lease: `None` when a reach finds
    /// the range already held.
    async fn resolve(
        &self,
        store: &dyn StoreRead,
        scope: &SyncScope,
        cursor: Option<&SyncState>,
    ) -> Result<Option<Resolved>, SyncError> {
        let expansion = store
            .expansion_window(scope)
            .await?
            .ok_or(SyncError::NoExpansionWindow)?;
        let windowed = self
            .provider
            .connection_info()
            .capabilities
            .windowed_events();
        let recorded = store.fetch_window(scope).await?;
        // A cursor with no recorded window was opened before windows were recorded, over one
        // the host chose then. The range the host shows is the best stand-in for it.
        let opened = recorded.unwrap_or(expansion.horizon);
        let (fetch, tag) = match self.plan {
            EventPlan::Follow => {
                let fetch = EventFetch {
                    window: opened,
                    windowed,
                    reopen: false,
                };
                (fetch, self.tag.clone())
            }
            EventPlan::Reach(range) => {
                let covered = !windowed || recorded.is_some_and(|window| window.contains(range));
                if cursor.is_some() && covered {
                    return Ok(None);
                }
                let fetch = EventFetch {
                    window: opened.union(range),
                    windowed,
                    reopen: windowed,
                };
                // A reopen lists the calendar as it is now, which is at least as new as the
                // state the recorded tag names, so the tag still says when nothing changed.
                (fetch, store.collection_tag(scope).await?)
            }
        };
        Ok(Some(Resolved {
            expansion,
            fetch,
            tag,
        }))
    }
}

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EventScope<'_, P> {
    type Halt = Covered;
    type Meta = ();
    type Object = Event;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.provider.event_scope(account, &self.calendar)
    }

    async fn prepare(
        &self,
        store: &dyn StoreRead,
        scope: &SyncScope,
        cursor: Option<&SyncState>,
    ) -> Result<Option<Covered>, SyncError> {
        let resolved = self.resolve(store, scope, cursor).await?;
        let halt = resolved.is_none().then_some(Covered);
        *self.resolved.lock().expect("resolved mutex poisoned") = resolved;
        Ok(halt)
    }

    fn collection_tag(&self) -> Option<CollectionTag> {
        self.resolved().tag
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Event, (), Covered>, ProviderError> {
        let fetch = self.resolved().fetch;
        let cursor = if fetch.reopen { None } else { cursor };
        let sync = self
            .provider
            .sync_events(account, &self.calendar, cursor, fetch.window)
            .await;
        sync.map(|sync| ScopeFetch::Proceed { sync, meta: () })
    }

    fn fetch_window(&self, sync: &ScopeSync<Event>) -> Option<Horizon> {
        let fetch = self.resolved().fetch;
        (fetch.windowed && sync.is_snapshot()).then_some(fetch.window)
    }

    fn derive(&self, sync: &ScopeSync<Event>) -> DerivedWrite {
        let window = self.resolved().expansion;
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
            match expand(event, &window.horizon, &window.zone) {
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
