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
use engine_store::DerivedWrite;

use crate::{ScopeFetch, ScopeSyncer, UnexpandableEvent, changed_objects};

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
/// `derive` cannot fail (its signature returns the rows, not a `Result`), but an event the
/// expander refuses materializes **no occurrences**: it is stored, yet renders nowhere on a
/// grid. Collecting those here rather than discarding them lets the caller report them, so the
/// absence is visible instead of silent.
pub(crate) struct EventScope<'p, P> {
    pub(crate) provider: &'p P,
    /// The calendar the scope is fetched through; any calendar of a shared scope answers for
    /// all of them.
    pub(crate) calendar: CalendarId,
    /// The window the store holds, never one a caller passed. See
    /// [`sync_calendar`](crate::sync_calendar).
    pub(crate) window: ExpansionWindow,
    /// The listing's tag for the calendar, read before the fetch, recorded with the cursor.
    pub(crate) tag: Option<CollectionTag>,
    /// How the provider is asked for the scope's events.
    pub(crate) fetch: EventFetch,
    pub(crate) unexpandable: Mutex<Vec<UnexpandableEvent>>,
}

/// How an [`EventScope`] asks its provider for events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventFetch {
    /// The transport fetches a calendar whole ([`sync_events`](Provider::sync_events)).
    Whole,
    /// The transport fetches inside a window
    /// ([`windowed_events`](engine_provider::Capabilities::windowed_events)): `window` opens
    /// any cursor it must open, and `reopen` opens a fresh one over it even where the scope
    /// has a cursor, which is how a range past the recorded window is fetched.
    Within { window: Horizon, reopen: bool },
}

impl EventFetch {
    /// How `provider` is asked for a scope whose recorded fetch window is `recorded`, expanded
    /// over `expansion`: inside the recorded window where there is one, else the expansion's,
    /// which is the range the host has asked to see.
    pub(crate) fn for_scope<P: Provider>(
        provider: &P,
        recorded: Option<Horizon>,
        expansion: &ExpansionWindow,
    ) -> Self {
        if provider.connection_info().capabilities.windowed_events() {
            Self::Within {
                window: recorded.unwrap_or(expansion.horizon),
                reopen: false,
            }
        } else {
            Self::Whole
        }
    }
}

impl<'p, P> EventScope<'p, P> {
    pub(crate) fn new(
        provider: &'p P,
        calendar: CalendarId,
        window: ExpansionWindow,
        tag: Option<CollectionTag>,
        fetch: EventFetch,
    ) -> Self {
        Self {
            provider,
            calendar,
            window,
            tag,
            fetch,
            unexpandable: Mutex::default(),
        }
    }

    pub(crate) fn into_unexpandable(self) -> Vec<UnexpandableEvent> {
        self.unexpandable
            .into_inner()
            .expect("unexpandable mutex poisoned")
    }
}

#[async_trait::async_trait]
impl<P: Provider> ScopeSyncer for EventScope<'_, P> {
    type Halt = core::convert::Infallible;
    type Meta = ();
    type Object = Event;

    fn scope(&self, account: &AccountId) -> SyncScope {
        self.provider.event_scope(account, &self.calendar)
    }

    fn collection_tag(&self) -> Option<&CollectionTag> {
        self.tag.as_ref()
    }

    async fn fetch(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> Result<ScopeFetch<Event, (), Self::Halt>, ProviderError> {
        let sync = match self.fetch {
            EventFetch::Whole => {
                self.provider
                    .sync_events(account, &self.calendar, cursor)
                    .await
            }
            EventFetch::Within { window, reopen } => {
                let cursor = if reopen { None } else { cursor };
                self.provider
                    .sync_events_within(account, &self.calendar, cursor, window)
                    .await
            }
        };
        sync.map(|sync| ScopeFetch::Proceed { sync, meta: () })
    }

    fn fetch_window(&self, sync: &ScopeSync<Event>) -> Option<Horizon> {
        match self.fetch {
            EventFetch::Within { window, .. } if sync.is_snapshot() => Some(window),
            _ => None,
        }
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
