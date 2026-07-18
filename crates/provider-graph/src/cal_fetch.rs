//! Calendar-list and event snapshot/delta fetch + paging for the Graph calendar
//! provider.
//!
//! Events sync through `calendarView/delta` — the one Graph calendar endpoint with a
//! real windowed delta (`graph.md`). It returns the series **master** (with
//! `patternedRecurrence`), standalone **single** events, the server's pre-expanded
//! **occurrences**, and per-instance **exceptions**. The engine stores a master + rule
//! and expands locally, so [`keep`] projects only `seriesMaster`/`singleInstance` and
//! drops `occurrence` (the engine re-expands the master) and `exception` (Graph v1.0
//! exposes no recurrence-id to key an override on — see the `graph.md` limitations). A
//! `@removed` entry is an inline tombstone, reusing the mail delta machinery.

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
    error::GraphError,
    json::{req_str, wrap_id},
    transport::GraphClient,
};

/// Cursor placeholder for an intermediate page (the drain ignores it until the final
/// page carries the `@odata.deltaLink`).
const PENDING_CURSOR: &str = "graph-cal-pending";

/// The date window a calendar sync covers: `calendarView` requires an explicit range,
/// and the returned `deltaLink` encodes it, so it is applied only to the initial
/// request. A host sizes it from its recurrence-expansion horizon (`providers.md`:
/// calendar coverage "may be inherently time-windowed").
#[derive(Debug, Clone, Copy)]
pub struct CalendarWindow {
    /// The inclusive lower bound (00:00:00 UTC of this date).
    pub start: CalendarDate,
    /// The exclusive upper bound (00:00:00 UTC of this date).
    pub end: CalendarDate,
}

impl CalendarWindow {
    /// A window spanning `[start, end)` — the date range `calendarView` covers.
    #[must_use]
    pub fn new(start: CalendarDate, end: CalendarDate) -> Self {
        Self { start, end }
    }
}

/// Fetches the account's calendars as a snapshot (`GET /me/calendars`), draining every
/// `@odata.nextLink` page.
pub(crate) async fn calendars(client: &GraphClient) -> Result<Vec<Calendar>, GraphError> {
    let mut calendars = Vec::new();
    let mut url = client.url("/calendars?$top=100");
    loop {
        let doc = client.get(&url).await?;
        for calendar in value_array(&doc, "calendars")? {
            calendars.push(calendar_from_json(calendar)?);
        }
        match odata_link(&doc, "@odata.nextLink") {
            Some(next) => url = next,
            None => break,
        }
    }
    Ok(calendars)
}

/// Fetches one page of the bound calendar's events via `calendarView/delta`. `window`
/// bounds the **initial** request; a continuation follows the server's link (which
/// encodes the window). `display_zone` (an IANA name) rides a `Prefer: outlook.timezone`
/// header so Graph returns each event's wall clock in that zone rather than UTC — the
/// authoring-zone form the engine needs to expand a recurring master DST-correctly
/// (`calendar-semantics.md`). It must be re-sent on every request (headers are not
/// encoded in the deltaLink). Only masters/singles are projected (see [`keep`]).
pub(crate) async fn events_page(
    client: &GraphClient,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    window: CalendarWindow,
    display_zone: &str,
) -> Result<SyncPage<Event>, GraphError> {
    let kind = if cursor.is_none() {
        SyncKind::Snapshot
    } else {
        SyncKind::Delta
    };
    let prefer = format!("outlook.timezone=\"{display_zone}\"");
    let doc = client
        .get_with_prefer(
            &page_url(client, calendar, cursor, page, window),
            Some(&prefer),
        )
        .await?;

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut present = Vec::new();
    for entry in value_array(&doc, "calendarView delta")? {
        if entry.get("@removed").is_some() {
            removed.push(entry_key(entry)?);
            continue;
        }
        if !keep(entry) {
            // A server-expanded `occurrence`, or an `exception` we cannot key — dropped.
            continue;
        }
        let event = event_from_json(entry, calendar)?;
        if kind == SyncKind::Snapshot {
            present.push(event.id.key().clone());
        }
        changed.push(event);
    }

    let next_page = odata_link(&doc, "@odata.nextLink").map(PageToken::new);
    let next_cursor = match odata_link(&doc, "@odata.deltaLink") {
        Some(delta) => SyncState::new(delta),
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

/// Whether a `calendarView` entry is projected: a series master or a standalone single
/// event. A pre-expanded `occurrence` and a per-instance `exception` are dropped.
fn keep(entry: &Value) -> bool {
    matches!(
        entry.get("type").and_then(Value::as_str),
        Some("seriesMaster" | "singleInstance") | None
    )
}

/// The URL for the next page: a `@odata.nextLink` continuation, else the delta `cursor`,
/// else the calendar's first `calendarView/delta` call carrying the window.
fn page_url(
    client: &GraphClient,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    window: CalendarWindow,
) -> String {
    if let Some(page) = page {
        page.as_str().to_owned()
    } else if let Some(cursor) = cursor {
        cursor.as_str().to_owned()
    } else {
        client.url(&format!(
            "/calendars/{}/calendarView/delta?startDateTime={}T00:00:00Z&endDateTime={}T00:00:00Z",
            calendar.key().as_str(),
            window.start,
            window.end
        ))
    }
}

/// The `value` array of a Graph collection response, or a protocol error.
fn value_array<'a>(doc: &'a Value, what: &str) -> Result<&'a Vec<Value>, GraphError> {
    doc.get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| GraphError::protocol(format!("{what} response had no value array")))
}

/// The `ProviderKey` of a delta entry (its `id`).
fn entry_key(entry: &Value) -> Result<ProviderKey, GraphError> {
    wrap_id(ProviderKey::new(req_str(entry, "id")?), "event id")
}

/// An `@odata.*` link field as an owned absolute URL.
fn odata_link(doc: &Value, key: &str) -> Option<String> {
    doc.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
#[path = "cal_fetch_tests.rs"]
mod tests;
