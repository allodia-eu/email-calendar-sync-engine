//! Calendar-list and event snapshot/delta fetch + paging for the Graph calendar
//! provider.
//!
//! Events sync through `calendarView/delta` — the one Graph calendar endpoint with a
//! real windowed delta (`graph.md`). It returns the series **master** (with
//! `patternedRecurrence`), standalone **single** events, the server's pre-expanded
//! **occurrences**, and per-instance **exceptions**. The engine stores a master + rule
//! and expands locally, so an `occurrence` is dropped and an `exception` is set aside as
//! an override of its series ([`cal_override`](crate::cal_override)) rather than stored as
//! an object of its own. A `@removed` entry is an inline tombstone, reusing the mail delta
//! machinery.
//!
//! # A series master costs one more request, and is read in its own zone
//!
//! Two things a series needs are absent from the delta, both measured: an occurrence the
//! user **removed** appears only in the master's `cancelledOccurrences`, and a `$select`
//! naming it is silently dropped from any collection response (and would starve the
//! normalizer besides). So each `seriesMaster` on a page is re-read on its own — and that
//! read carries `Prefer: outlook.timezone` set to the series' **own**
//! `originalStartTimeZone` rather than the display zone, because Graph names an occurrence
//! by its date in the authoring zone and that name does not follow the header. Reading the
//! master in the same zone is what makes the override keys line up; the reasoning is in
//! [`cal_override`](crate::cal_override).

use engine_core::{
    calendar::{Calendar, Event},
    ids::{CalendarId, ProviderKey},
    sync::SyncState,
    time::CalendarDate,
};
use engine_provider::{PageToken, SyncKind, SyncPage};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::Value;

use crate::{
    cal_normalize::{calendar_from_json, event_from_json},
    cal_override::{self, PendingOverride},
    error::GraphError,
    json::{opt_str, req_str, wrap_id},
    transport::GraphClient,
};

/// How many series masters are re-read at once. A first sync meets every recurring event
/// in the window, and serializing those reads would show up as a slow first calendar sync;
/// the cap keeps the burst polite (the same trade `provider-google` makes per message).
const MAX_CONCURRENT_MASTER_READS: usize = 8;

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

/// One page of events, plus the occurrence-level entries on it.
///
/// The overrides ride beside the page rather than inside it because they cannot be applied
/// yet: an entry names its master by id, and the master may be on another page (see
/// [`cal_override`](crate::cal_override)).
#[derive(Debug)]
pub(crate) struct EventsPage {
    /// The masters and single events this page carried.
    pub(crate) page: SyncPage<Event>,
    /// The occurrence-level entries this page carried, unfolded.
    pub(crate) overrides: Vec<PendingOverride>,
}

/// Fetches one page of the bound calendar's events via `calendarView/delta`. `window`
/// bounds the **initial** request; a continuation follows the server's link (which
/// encodes the window). `display_zone` (an IANA name) rides a `Prefer: outlook.timezone`
/// header so Graph returns each event's wall clock in that zone rather than UTC — a
/// zoneless UTC reading expands a recurring master DST-incorrectly
/// (`calendar-semantics.md`). It must be re-sent on every request (headers are not
/// encoded in the deltaLink). A series master is then re-read in its own authoring zone
/// (see this module's header).
pub(crate) async fn events_page(
    client: &GraphClient,
    calendar: &CalendarId,
    cursor: Option<&SyncState>,
    page: Option<&PageToken>,
    window: CalendarWindow,
    display_zone: &str,
) -> Result<EventsPage, GraphError> {
    let kind = if cursor.is_none() {
        SyncKind::Snapshot
    } else {
        SyncKind::Delta
    };
    let doc = client
        .get_with_prefer(
            &page_url(client, calendar, cursor, page, window),
            Some(&prefer(display_zone)),
        )
        .await?;

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut overrides = Vec::new();
    let mut masters = Vec::new();
    for entry in value_array(&doc, "calendarView delta")? {
        if entry.get("@removed").is_some() {
            removed.push(entry_key(entry)?);
            continue;
        }
        match entry.get("type").and_then(Value::as_str) {
            // Server-expanded; the engine re-expands the master itself, so it is dropped.
            Some("occurrence") => {}
            // An occurrence somebody changed. Not an object of its own to this engine —
            // it is an exception *of* its series — so it is folded in once the pass is in.
            Some("exception") => overrides.push(cal_override::pending_override(entry)?),
            // Deferred: it needs a second request, which the whole page then runs at once.
            Some("seriesMaster") => masters.push(entry.clone()),
            _ => changed.push(event_from_json(entry, calendar)?),
        }
    }
    for (event, cancelled) in read_masters(client, calendar, masters, display_zone).await? {
        changed.push(event);
        overrides.extend(cancelled);
    }

    let present = if kind == SyncKind::Snapshot {
        changed.iter().map(|e| e.id.key().clone()).collect()
    } else {
        Vec::new()
    };
    let next_page = odata_link(&doc, "@odata.nextLink").map(PageToken::new);
    let next_cursor = match odata_link(&doc, "@odata.deltaLink") {
        Some(delta) => SyncState::new(delta),
        None => cursor
            .cloned()
            .unwrap_or_else(|| SyncState::new(PENDING_CURSOR)),
    };
    Ok(EventsPage {
        overrides,
        page: SyncPage {
            kind,
            changed,
            patched: Vec::new(),
            removed,
            present,
            next_page,
            next_cursor,
            total: None,
        },
    })
}

/// Re-reads each series master in its own zone, up to [`MAX_CONCURRENT_MASTER_READS`] at a
/// time, returning each with the occurrences it says were removed.
async fn read_masters(
    client: &GraphClient,
    calendar: &CalendarId,
    masters: Vec<Value>,
    display_zone: &str,
) -> Result<Vec<(Event, Vec<PendingOverride>)>, GraphError> {
    stream::iter(masters)
        .map(|entry| async move { read_master(client, calendar, entry, display_zone).await })
        .buffered(MAX_CONCURRENT_MASTER_READS)
        .try_collect()
        .await
}

/// Re-reads one series master's start, end and `cancelledOccurrences` in the zone the
/// series was authored in.
///
/// The re-read start/end **replace** the delta's, so the stored master and the ids Graph
/// names its occurrences by share one zone — without which a series authored outside the
/// display zone keys its overrides on the wrong day
/// ([`cal_override`](crate::cal_override)). They are merged into the delta entry rather
/// than applied to the projection, so the raw payload preserved beside it says the same
/// thing the projection does.
async fn read_master(
    client: &GraphClient,
    calendar: &CalendarId,
    mut entry: Value,
    display_zone: &str,
) -> Result<(Event, Vec<PendingOverride>), GraphError> {
    let key = entry_key(&entry)?;
    // A series with no authoring zone of its own has nothing to disagree with the display
    // zone, so the header the rest of the pass uses stands.
    let zone = opt_str(&entry, "originalStartTimeZone")
        .unwrap_or(display_zone)
        .to_owned();
    let doc = client
        .get_with_prefer(
            &client.url(&format!(
                "/events/{}?$select=start,end,cancelledOccurrences",
                key.as_str()
            )),
            Some(&prefer(&zone)),
        )
        .await?;

    for endpoint in ["start", "end"] {
        if let Some(value) = doc.get(endpoint) {
            entry[endpoint] = value.clone();
        }
    }
    Ok((
        event_from_json(&entry, calendar)?,
        cal_override::cancellations(&key, &doc)?,
    ))
}

/// The `Prefer` header value asking Graph for wall clocks in `zone`.
fn prefer(zone: &str) -> String {
    format!("outlook.timezone=\"{zone}\"")
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
