//! Calendar-list and event snapshot/delta fetch + paging for the Google calendar
//! provider.
//!
//! Events sync through `events.list` with a per-calendar `nextSyncToken` — Google's
//! incremental cursor. Unlike Graph's *mandatory* `calendarView` window, the time window
//! (`timeMin`/`timeMax`) is **optional** and applies only to the initial (snapshot)
//! request (a `syncToken` request cannot also carry a window). `singleEvents=false`
//! returns the series **master** (with its `RRULE`) and standalone **single** events; the
//! engine expands the master locally, so a per-instance override (`recurringEventId` set)
//! is dropped (deferred — `calendar-semantics.md`). A `status:"cancelled"` entry is a
//! tombstone. A `410` on a stale `syncToken` classifies as `NeedsResync`, restarting the
//! pass as a snapshot (the same mechanism as Gmail's history-expiry).

use engine_core::{
    calendar::{Calendar, Event},
    ids::{CalendarId, ProviderKey},
    sync::SyncState,
    time::CalendarDate,
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use serde_json::Value;

use crate::{
    cal_normalize::{calendar_from_json, event_from_json},
    error::GoogleError,
    json::{opt_str, req_str, wrap_id},
    transport::GoogleClient,
};

/// The Google Calendar v3 API root.
const CALENDAR_BASE: &str = "/calendar/v3";

/// Cursor placeholder for an intermediate page (the drain ignores it until the final page
/// carries the `nextSyncToken`).
const PENDING_CURSOR: &str = "google-cal-pending";

/// The optional date window a calendar snapshot covers (`timeMin`/`timeMax`). A host
/// sizes it from its recurrence-expansion horizon; unset syncs the whole calendar.
#[derive(Debug, Clone, Copy)]
pub struct CalendarWindow {
    /// The inclusive lower bound (00:00:00 UTC of this date).
    pub start: CalendarDate,
    /// The exclusive upper bound (00:00:00 UTC of this date).
    pub end: CalendarDate,
}

impl CalendarWindow {
    /// A window spanning `[start, end)`.
    #[must_use]
    pub fn new(start: CalendarDate, end: CalendarDate) -> Self {
        Self { start, end }
    }
}

/// Fetches the account's calendars as a snapshot (`calendarList.list`), draining every
/// `nextPageToken` page.
pub(crate) async fn calendars(client: &GoogleClient) -> Result<Vec<Calendar>, GoogleError> {
    let mut calendars = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut url = format!("{CALENDAR_BASE}/users/me/calendarList?maxResults=250");
        if let Some(token) = &page {
            use core::fmt::Write as _;
            let _ = write!(url, "&pageToken={token}");
        }
        let doc = client.get(&client.url(&url)).await?;
        for entry in array(&doc, "items", "calendarList")? {
            calendars.push(calendar_from_json(entry)?);
        }
        match opt_str(&doc, "nextPageToken") {
            Some(token) => page = Some(token.to_owned()),
            None => break,
        }
    }
    Ok(calendars)
}

/// Fetches one page of the bound calendar's events via `events.list`. `window` bounds the
/// **initial** snapshot request only; a delta (`cursor`) and a continuation (`page`)
/// carry the server's tokens.
pub(crate) async fn events_page(
    client: &GoogleClient,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    window: Option<CalendarWindow>,
) -> Result<SyncPage<Event>, GoogleError> {
    let kind = if cursor.is_none() {
        SyncKind::Snapshot
    } else {
        SyncKind::Delta
    };
    let doc = client
        .get(&page_url(client, calendar, cursor, page, window))
        .await?;
    // The response's top-level zone is the calendar's default, used for an endpoint that
    // omits its own timeZone.
    let default_zone = opt_str(&doc, "timeZone");

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut present = Vec::new();
    for entry in array(&doc, "items", "events.list")? {
        if opt_str(entry, "status") == Some("cancelled") {
            removed.push(entry_key(entry)?);
            continue;
        }
        // A per-instance override (a modified exception) — dropped; the engine expands
        // the master itself (per-instance override reconciliation is deferred).
        if entry.get("recurringEventId").is_some() {
            continue;
        }
        let event = event_from_json(entry, calendar, default_zone)?;
        if kind == SyncKind::Snapshot {
            present.push(event.id.key().clone());
        }
        changed.push(event);
    }

    let next_page = opt_str(&doc, "nextPageToken").map(PageToken::new);
    let next_cursor = match opt_str(&doc, "nextSyncToken") {
        Some(token) => SyncState::new(token),
        None => cursor
            .cloned()
            .unwrap_or_else(|| SyncState::new(PENDING_CURSOR)),
    };
    Ok(SyncPage {
        kind,
        changed,
        removed,
        present,
        next_page,
        next_cursor,
        total: None,
    })
}

/// The `events.list` URL: a `pageToken` continuation, else a `syncToken` delta, else the
/// calendar's first request (`singleEvents=false`, optionally windowed by `timeMin`/`timeMax`).
fn page_url(
    client: &GoogleClient,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    window: Option<CalendarWindow>,
) -> String {
    let base = format!(
        "{CALENDAR_BASE}/calendars/{}/events",
        calendar.key().as_str()
    );
    if let Some(page) = page {
        return client.url(&format!("{base}?pageToken={}", page.as_str()));
    }
    if let Some(cursor) = cursor {
        return client.url(&format!("{base}?syncToken={}", cursor.as_str()));
    }
    let mut url = format!("{base}?singleEvents=false&maxResults=250");
    if let Some(window) = window {
        use core::fmt::Write as _;
        let _ = write!(
            url,
            "&timeMin={}T00:00:00Z&timeMax={}T00:00:00Z",
            window.start, window.end
        );
    }
    client.url(&url)
}

/// The named array field of a response, or a protocol error.
fn array<'a>(doc: &'a Value, key: &str, what: &str) -> Result<&'a Vec<Value>, GoogleError> {
    doc.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| GoogleError::protocol(format!("{what} response had no {key} array")))
}

/// The `ProviderKey` of an entry (its `id`).
fn entry_key(entry: &Value) -> Result<ProviderKey, GoogleError> {
    wrap_id(ProviderKey::new(req_str(entry, "id")?), "event id")
}

#[cfg(test)]
#[path = "cal_fetch_tests.rs"]
mod tests;
