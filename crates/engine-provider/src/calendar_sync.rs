//! The [`CalendarSync`] half of the provider seam: which scopes an account's calendars and
//! events sync under, and fetching them.
//!
//! A supertrait of [`Provider`](crate::Provider) beside
//! [`CalendarWrites`](crate::CalendarWrites), so the trait that is otherwise about mail keeps
//! to mail. A caller bounded on `P: Provider` gets these methods with the bound; one holding
//! a concrete adapter imports this trait to call them.
//!
//! The scopes default to the JMAP ones and the fetches to rejecting, so an adapter without
//! calendars states that with an empty impl and a capability set that says the same
//! ([`Capabilities::calendars`]).

use async_trait::async_trait;
use engine_core::{
    calendar::Event,
    ids::{AccountId, CalendarId},
    sync::{JmapDataType, SyncScope, SyncState},
    time::Horizon,
};

use crate::{CalendarListing, ProviderResult, ScopeSync, error::unsupported, sync::jmap_scope};
// Named only by intra-doc links, which rustdoc resolves against this module's scope.
#[allow(unused_imports, reason = "named by intra-doc links only")]
use crate::{Capabilities, ProviderError};

/// The calendar-sync scopes and fetches every adapter answers, rejecting by default.
///
/// See the module header for why this is its own trait.
#[async_trait]
pub trait CalendarSync: Send + Sync {
    /// The scope the account's calendars sync under. Defaults to the JMAP
    /// `(account, Calendar)` scope; non-JMAP providers override.
    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        jmap_scope(account, JmapDataType::Calendar)
    }

    /// The scope `calendar`'s events sync under.
    ///
    /// One provider serves every calendar of the account it signed in to, so this is asked per
    /// calendar. Where the protocol's event delta is per calendar (CalDAV, Graph, Google) each
    /// calendar has a scope of its own; where it is account-wide (JMAP's `CalendarEvent` state)
    /// every calendar answers the **same** scope, and the account's events sync once under it.
    /// Defaults to that JMAP scope; non-JMAP providers override.
    fn event_scope(&self, account: &AccountId, calendar: &CalendarId) -> SyncScope {
        let _ = calendar;
        jmap_scope(account, JmapDataType::CalendarEvent)
    }

    /// Fetches the account's calendar collections since `cursor`. Providers
    /// advertising [`Capabilities::calendars`] override this.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn sync_calendars(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<CalendarListing> {
        let _ = (account, cursor);
        Err(unsupported("calendar sync"))
    }

    /// Fetches the events of the scope [`event_scope`](Self::event_scope) names for `calendar`,
    /// since `cursor`. Where several calendars share one scope, any of them answers for all.
    /// Providers advertising [`Capabilities::calendars`] override this.
    ///
    /// `window` is the date range the engine wants the scope to hold. A transport that fetches
    /// inside one ([`Capabilities::windowed_events`]) opens any cursor it must open (none yet, or
    /// one the server expired) over it, and a delta keeps the window its cursor was opened over;
    /// one that fetches a calendar whole ignores it.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`]; the default returns
    /// [`FailureClass::InvalidState`](engine_core::error::FailureClass::InvalidState).
    async fn sync_events(
        &self,
        account: &AccountId,
        calendar: &CalendarId,
        cursor: Option<&SyncState>,
        window: Horizon,
    ) -> ProviderResult<ScopeSync<Event>> {
        let _ = (account, calendar, cursor, window);
        Err(unsupported("calendar sync"))
    }
}
