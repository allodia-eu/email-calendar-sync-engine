//! The calendar [`Provider`] implementation: a Google client bound to one calendar for
//! events, with the calendar list synced at the account level.
//!
//! Like [`GmailProvider`](crate::GmailProvider) is account-global for mail, a
//! [`GoogleCalendarProvider`] is **bound to one calendar**: its
//! [`event_scope`](Provider::event_scope) names that calendar
//! ([`SyncScope::GoogleCalendar`]) and syncs its `events.list`, while the calendar list
//! syncs under the per-account [`SyncScope::GoogleCalendarList`]. The cross-calendar
//! fan-out is the orchestrator's job. Unlike Graph, the time window is **optional**, and
//! Google is IANA-native (no display-zone request header).

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    error::FailureClass,
    ids::{AccountId, CalendarId},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, EventDeletion, EventDraft, EventEdit, EventWriteReceipt,
    PageToken, Provider, ProviderError, ProviderResult, ScopeSync, SyncKind, WriteGuard,
};

use crate::{
    cal_fetch::{self, CalendarWindow},
    cal_write,
    transport::GoogleClient,
};

/// The calendar list is re-discovered as a snapshot each pass (`calendarList.list`), so it
/// carries no provider cursor of its own — like Gmail's label list.
const CALENDAR_LIST_CURSOR: &str = "google-calendars";

/// A Google Calendar read/sync provider bound to one calendar.
///
/// Construct one with [`GoogleCalendarProvider::new`] from a connected [`GoogleClient`]
/// and the calendar to bind; optionally set a snapshot date [`window`](CalendarWindow)
/// with [`with_window`](Self::with_window) (Google's window is optional, unlike Graph's).
pub struct GoogleCalendarProvider {
    client: GoogleClient,
    calendar: CalendarId,
    window: Option<CalendarWindow>,
    capabilities: Capabilities,
}

impl core::fmt::Debug for GoogleCalendarProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoogleCalendarProvider")
            .field("calendar", &self.calendar.key().as_str())
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl GoogleCalendarProvider {
    /// Binds a connected client to one calendar for event read/sync and writes.
    ///
    /// Google *enforces* the lost-update guard — a stale `If-Match` ETag is a `412` on a
    /// write — so, like Graph and unlike JMAP, it advertises [`WriteGuard::Enforced`].
    #[must_use]
    pub fn new(client: GoogleClient, calendar: CalendarId) -> Self {
        Self {
            client,
            calendar,
            window: None,
            capabilities: Capabilities::none()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Enforced),
        }
    }

    /// The bound calendar's id as a path segment.
    fn calendar_id(&self) -> &str {
        self.calendar.key().as_str()
    }

    /// Windows the initial (snapshot) event enumeration to `[start, end)` via
    /// `timeMin`/`timeMax`. A delta ignores it (the `syncToken` encodes the scope).
    #[must_use]
    pub fn with_window(mut self, window: CalendarWindow) -> Self {
        self.window = Some(window);
        self
    }
}

#[async_trait]
impl Provider for GoogleCalendarProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GoogleCalendarList {
            account: account.clone(),
        }
    }

    fn event_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GoogleCalendar {
            account: account.clone(),
            calendar: self.calendar.clone(),
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        // `calendarList.list` is a full snapshot every pass, so every calendar is present.
        let calendars = cal_fetch::calendars(&self.client).await?;
        let present = calendars.iter().map(|c| c.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(calendars, present),
            SyncState::new(CALENDAR_LIST_CURSOR),
        ))
    }

    async fn sync_events(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Event>> {
        // Drain every `events.list` page into one update. A snapshot reconciles (its
        // `present` set tombstones absent rows); a delta carries explicit removals.
        let mut cursor = cursor;
        let mut page_token: Option<PageToken> = None;
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let mut present = BTreeSet::new();
        let mut kind: Option<SyncKind> = None;
        let next_cursor = loop {
            let page = match cal_fetch::events_page(
                &self.client,
                &self.calendar,
                cursor,
                page_token.as_ref(),
                self.window,
            )
            .await
            {
                Ok(page) => page,
                // Google expired the stored syncToken (`410`): drop it and restart as a
                // full snapshot. Only before the first page is committed (`page_token`
                // still `None`) — after that the mode is fixed, exactly like Gmail.
                Err(err)
                    if cursor.is_some()
                        && page_token.is_none()
                        && err.failure_class() == FailureClass::NeedsResync =>
                {
                    cursor = None;
                    continue;
                }
                Err(err) => return Err(err.into()),
            };
            kind.get_or_insert(page.kind);
            changed.extend(page.changed);
            removed.extend(page.removed);
            present.extend(page.present);
            if page.next_page.is_none() {
                break page.next_cursor;
            }
            page_token = page.next_page;
        };
        let update = match kind.unwrap_or(SyncKind::Delta) {
            SyncKind::Snapshot => SyncUpdate::snapshot(changed, present),
            SyncKind::Delta => SyncUpdate::delta(changed, removed),
        };
        Ok(ScopeSync::new(update, next_cursor))
    }

    async fn create_event(
        &self,
        _account: &AccountId,
        draft: &EventDraft,
    ) -> ProviderResult<EventWriteReceipt> {
        // A draft naming a different calendar is refused rather than silently written to
        // the bound one — this provider is calendar-bound, like `GraphCalendarProvider`.
        if draft.calendar != self.calendar {
            return Err(ProviderError::invalid_state(format!(
                "draft targets calendar {:?}, but this provider is bound to {:?}",
                draft.calendar.key().as_str(),
                self.calendar_id()
            )));
        }
        cal_write::create_event(&self.client, self.calendar_id(), draft).await
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        cal_write::patch_event(&self.client, self.calendar_id(), base, edit).await
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        cal_write::delete_event(&self.client, self.calendar_id(), deletion).await
    }
}

#[cfg(test)]
#[path = "calendar_provider_tests.rs"]
mod tests;
