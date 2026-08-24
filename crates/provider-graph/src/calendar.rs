//! The calendar [`Provider`] implementation: a Microsoft Graph client bound to one
//! calendar for events, with the calendar list synced at the account level.
//!
//! Like [`GraphProvider`](crate::GraphProvider) for mail (and `CalDavProvider` for
//! CalDAV), a [`GraphCalendarProvider`] is **bound to one calendar**: its
//! [`event_scope`](Provider::event_scope) names that calendar
//! ([`SyncScope::GraphCalendar`]) and syncs its `calendarView/delta`, while the calendar
//! list syncs under the per-account [`SyncScope::GraphCalendarList`]. The cross-calendar
//! fan-out is the orchestrator's job. It advertises calendar read/sync **and** writes
//! guarded by `If-Match` ([`WriteGuard::Enforced`]).

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    calendar::{Calendar, Event},
    error::FailureClass,
    ids::{AccountId, CalendarId},
    sync::{SyncScope, SyncState, SyncUpdate},
    time::TimeZoneId,
};
use engine_provider::{
    Capabilities, ConnectionInfo, EventDeletion, EventDraft, EventEdit, EventRsvp,
    EventWriteReceipt, PageToken, Provider, ProviderError, ProviderResult, RsvpControls, ScopeSync,
    SyncKind, WriteGuard,
};

use crate::{
    cal_fetch::{self, CalendarWindow, EventsPage},
    cal_override, cal_write,
    transport::GraphClient,
};

/// The calendar list is re-discovered as a snapshot each pass (`GET /me/calendars`), so
/// it carries no provider cursor of its own — like IMAP's/CalDAV's collection list.
const CALENDAR_LIST_CURSOR: &str = "graph-calendars";

/// What a Graph RSVP can and cannot control.
///
/// Both surrounding controls are native (`comment`, `sendResponse`). The guard is **not**:
/// the RSVP is a action endpoint (`POST /events/{id}/accept`) that accepts no `If-Match`,
/// unlike the `PATCH` this adapter uses for edits — so the enforced guard it promises for
/// writes does not extend to answering. Declared once, and used both to advertise and to
/// enforce, so the two can never disagree.
const GRAPH_RSVP: RsvpControls = RsvpControls {
    comment: true,
    suppress_notification: true,
    guard: WriteGuard::Absent,
};

/// A Microsoft Graph calendar read/sync/write provider bound to one calendar.
///
/// Construct one with [`GraphCalendarProvider::new`] from a connected
/// [`GraphClient`](crate::GraphClient), the calendar to bind, and the date [`window`]
/// its `calendarView` covers (Graph calendar sync is inherently time-windowed).
///
/// [`window`]: CalendarWindow
pub struct GraphCalendarProvider {
    client: GraphClient,
    calendar: CalendarId,
    window: CalendarWindow,
    display_zone: TimeZoneId,
    capabilities: Capabilities,
}

impl core::fmt::Debug for GraphCalendarProvider {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphCalendarProvider")
            .field("calendar", &self.calendar.key().as_str())
            .field("window", &self.window)
            .field("display_zone", &self.display_zone.as_str())
            .finish_non_exhaustive()
    }
}

impl GraphCalendarProvider {
    /// Binds a connected client to one calendar for events, covering the date `window`
    /// and reading event times in `display_zone` (the host's IANA display/home zone —
    /// Graph returns UTC otherwise, which would expand a recurring series
    /// DST-incorrectly; `calendar-semantics.md`).
    ///
    /// Graph *enforces* the lost-update guard — a stale `If-Match` ETag is a `412` on a
    /// write — so unlike JMAP it advertises [`WriteGuard::Enforced`].
    ///
    /// It also advertises [`Capabilities::calendar_scheduling`]: Exchange sends the iTIP
    /// `REQUEST`/`REPLY`/`CANCEL` a meeting write implies, and there is no opt-out a client
    /// can reach — the only control is `sendResponse` on an RSVP, which chooses whether the
    /// *organizer* is told, not whether the server is the one telling them. Unlike CalDAV
    /// there is nothing to discover: it is a property of the service, not of the tenant.
    #[must_use]
    pub fn new(
        client: GraphClient,
        calendar: CalendarId,
        window: CalendarWindow,
        display_zone: TimeZoneId,
    ) -> Self {
        Self {
            client,
            calendar,
            window,
            display_zone,
            capabilities: Capabilities::none()
                .with_calendars()
                .with_calendar_writes(WriteGuard::Enforced)
                .with_calendar_rsvp(GRAPH_RSVP)
                .with_calendar_scheduling(),
        }
    }

    /// The bound calendar's home-relative path (`/calendars/{id}`), rooted at the
    /// principal by [`GraphClient::url`].
    fn calendar_path(&self) -> String {
        format!("/calendars/{}", self.calendar.key().as_str())
    }
}

#[async_trait]
impl Provider for GraphCalendarProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }

    fn calendar_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphCalendarList {
            account: account.clone(),
        }
    }

    fn event_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphCalendar {
            account: account.clone(),
            calendar: self.calendar.clone(),
        }
    }

    async fn sync_calendars(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Calendar>> {
        // `GET /me/calendars` is a full snapshot every pass, so every calendar is present.
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
        // Drain every `calendarView/delta` page into one update. A snapshot reconciles
        // (its `present` set tombstones absent rows); a delta carries explicit removals.
        let mut cursor = cursor;
        let mut page_token: Option<PageToken> = None;
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        let mut present = BTreeSet::new();
        let mut overrides = Vec::new();
        let mut kind: Option<SyncKind> = None;
        let next_cursor = loop {
            let page = match cal_fetch::events_page(
                &self.client,
                &self.calendar,
                cursor,
                page_token.as_ref(),
                self.window,
                self.display_zone.as_str(),
            )
            .await
            {
                Ok(page) => page,
                // Graph expired the stored deltaLink (`410`): drop it and restart as a
                // full snapshot. Only before the first page is committed (`page_token`
                // still `None`) — after that the mode is fixed, exactly like mail.
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
            let EventsPage {
                page,
                overrides: page_overrides,
            } = page;
            kind.get_or_insert(page.kind);
            changed.extend(page.changed);
            removed.extend(page.removed);
            present.extend(page.present);
            overrides.extend(page_overrides);
            if page.next_page.is_none() {
                break page.next_cursor;
            }
            page_token = page.next_page;
        };
        // Only now: an exception names its master by id, and the master may have been on
        // any page of this pass — or, on a delta, on none of them.
        cal_override::fold_into(&mut changed, overrides);
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
        // the bound one — this provider is calendar-bound, like `CalDavProvider`.
        if draft.calendar != self.calendar {
            return Err(ProviderError::invalid_state(format!(
                "draft targets calendar {:?}, but this provider is bound to {:?}",
                draft.calendar.key().as_str(),
                self.calendar.key().as_str()
            )));
        }
        cal_write::create_event(&self.client, &self.calendar_path(), draft).await
    }

    async fn patch_event(
        &self,
        _account: &AccountId,
        base: &Event,
        edit: &EventEdit,
    ) -> ProviderResult<EventWriteReceipt> {
        cal_write::patch_event(&self.client, base, edit).await
    }

    async fn rsvp_event(
        &self,
        _account: &AccountId,
        base: &Event,
        rsvp: &EventRsvp,
    ) -> ProviderResult<EventWriteReceipt> {
        GRAPH_RSVP.accept(rsvp)?;
        cal_write::rsvp_event(&self.client, base, rsvp).await
    }

    async fn delete_event(
        &self,
        _account: &AccountId,
        _base: Option<&Event>,
        deletion: &EventDeletion,
    ) -> ProviderResult<()> {
        cal_write::delete_event(&self.client, deletion).await
    }
}

#[cfg(test)]
#[path = "calendar_provider_tests.rs"]
mod tests;
