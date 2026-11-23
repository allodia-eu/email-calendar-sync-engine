//! Importing events into CalDAV calendars (the `import` feature, `providers.md` → "Import").
//!
//! Each event is one calendar object, stored by one `PUT`:
//!
//! - **It tells nobody.** The object goes through [`engine_ical::import_object`], which marks every
//!   `ORGANIZER` and `ATTENDEE`, overrides included, `SCHEDULE-AGENT=CLIENT` (RFC 6638 §7.1), so a
//!   server that schedules sends no invitation, reply or cancellation for it. Proven live against
//!   Stalwart with a control that does send (`tests/common/import.rs`).
//! - **A `UID` the calendar holds is already there, never stored twice.** RFC 4791 §5.3.2.1 has the
//!   server refuse a second object with a `UID` (`no-uid-conflict`): Stalwart does with a `403` and
//!   Nextcloud with a `409`, each naming the existing object. SabreDAV's stock backend stores the
//!   duplicate (`docs/spec-deviations.md`, SABREDAV-1), and nothing a server says tells the two
//!   kinds apart, so the `UID` is looked up first (`calendar-query`) on every server. The refusals
//!   stay mapped too, for an object that appears between the lookup and the `PUT`.
//! - **A retry meets its own write.** The object is named after its `UID`
//!   ([`CalDavProvider::event_href`]) and created under `If-None-Match: *`, so a second attempt is
//!   refused `412` and answered as already there.
//! - **The point of no return is the `PUT` going out**: a `PUT` that may have reached the server
//!   and got no answer of its own is "may be stored" (`CalDavError::after_write`).
//!
//! The import cursor is the calendar's `sync-token`, the token a `sync-collection` delta starts
//! from, or its CTag where the server has no report, in the form [`crate::ctag_sync`] reads.
//! Errors carry a class and an HTTP status, never what the server said
//! ([`CalDavError::content_free`]).

use std::{collections::BTreeMap, num::NonZeroU64};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, CalendarId, EventId},
    sync::SyncState,
    version::{ETag, RevisionTokens},
};
use engine_ical::import_object;
use engine_provider::{
    ProviderError, ProviderResult,
    import::{CalendarImportLimits, CalendarImporter, EventImport, ImportedEvent},
};

use crate::{
    CalDavProvider,
    ctag_sync::ctag_cursor,
    dav::{MultiStatus, has_precondition},
    error::CalDavError,
    href::collection_href,
    precondition::precondition_href,
    request::{CALENDAR_IMPORT_PROPFIND, uid_query},
    transport::{DavExecutor, DavMethod, Precondition, WriteRequest},
};

/// The iCalendar media type an event is stored as.
const ICALENDAR: &str = "text/calendar; charset=utf-8";

/// The CalDAV preconditions (RFC 4791 §5.3.2.1) a server refuses one object by. A `412` naming
/// one is that object's refusal (Stalwart answers a malformed object `412`
/// `valid-calendar-object-resource`), where a bare `412` is `If-None-Match` meeting an object.
const OBJECT_PRECONDITIONS: [&str; 9] = [
    "supported-calendar-data",
    "valid-calendar-data",
    "valid-calendar-object-resource",
    "supported-calendar-component",
    "max-resource-size",
    "min-date-time",
    "max-date-time",
    "max-instances",
    "max-attendees-per-instance",
];

#[async_trait]
impl CalendarImporter for CalDavProvider {
    async fn calendar_import_limits(
        &self,
        _account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<CalendarImportLimits> {
        let props = properties(self.executor(), calendar)
            .await
            .map_err(|err| err.content_free("reading the calendar's limits"))?;
        Ok(limits_of(&props))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        calendar: &CalendarId,
    ) -> ProviderResult<SyncState> {
        let props = properties(self.executor(), calendar)
            .await
            .map_err(|err| err.content_free("reading the calendar's cursor"))?;
        let first = props.responses.first();
        let value = |name| {
            first
                .and_then(|response| response.props.get(name))
                .filter(|value| !value.is_empty())
        };
        if let Some(token) = value("sync-token") {
            return Ok(SyncState::new(token));
        }
        value("getctag").map(ctag_cursor).ok_or_else(|| {
            ProviderError::permanent(
                "reading the calendar's cursor: it offers neither a sync-token nor a CTag",
            )
        })
    }

    async fn import_events(
        &self,
        _account: &AccountId,
        batch: &[EventImport],
    ) -> Vec<ProviderResult<ImportedEvent>> {
        let mut limits: BTreeMap<&CalendarId, CalendarImportLimits> = BTreeMap::new();
        let mut answers = Vec::with_capacity(batch.len());
        for event in batch {
            if !limits.contains_key(&event.calendar) {
                // A calendar whose limits cannot be read states none here; the server still
                // refuses what it will not take, and that refusal answers the event.
                let stated = properties(self.executor(), &event.calendar)
                    .await
                    .map_or_else(|_| CalendarImportLimits::new(), |props| limits_of(&props));
                limits.insert(&event.calendar, stated);
            }
            let answer = match limits[&event.calendar].check(event) {
                Ok(()) => self.import_one(event).await,
                Err(refused) => Err(refused),
            };
            answers.push(answer);
        }
        answers
    }
}

impl CalDavProvider {
    /// Imports one event: prepare it, look its `UID` up, then `PUT` it where nothing is.
    async fn import_one(&self, event: &EventImport) -> ProviderResult<ImportedEvent> {
        let object = import_object(event.ical.as_str()).map_err(|_| {
            ProviderError::item_unavailable(
                "importing an event: the object is not one event with its overrides",
            )
        })?;
        let href = self
            .event_href(&event.calendar, &object.uid)
            .map_err(|err| err.content_free("importing an event"))?;
        let collection = collection_href(event.calendar.as_str())
            .map_err(|err| err.content_free("importing an event"))?;
        if let Some(existing) = holder_of(self.executor(), &collection, object.uid.as_str()).await?
        {
            return Ok(ImportedEvent::existing(Some(existing)));
        }
        let response = self
            .executor()
            .send_write(WriteRequest {
                method: DavMethod::Put,
                href: href.as_str().to_owned(),
                content_type: Some(ICALENDAR),
                precondition: Precondition::IfNoneMatch,
                body: object.ical.as_str().to_owned(),
            })
            .await
            .map_err(|err| err.content_free("importing an event"))?;
        let refusal = |status: u16| {
            ProviderError::item_unavailable(format!(
                "importing an event: the server refused it (HTTP {status})"
            ))
        };
        match response.status {
            200..=299 => Ok(ImportedEvent::created(
                href,
                response.etag.map_or_else(RevisionTokens::none, |etag| {
                    RevisionTokens::from_etag(ETag::new(etag))
                }),
            )),
            // Our own name taken: an earlier attempt of this import stored it.
            412 if !names_an_object_precondition(&response.body) => {
                Ok(ImportedEvent::existing(Some(href)))
            }
            // RFC 4791's own refusal of a second `UID`, which Stalwart sends as a `403` and
            // Nextcloud as a `409`, both naming the object that holds it. One that named none
            // is asked for.
            403 | 409 if has_precondition(&response.body, "no-uid-conflict") => {
                let named = precondition_href(&response.body, "no-uid-conflict");
                let holder = match named {
                    Some(holder) => Some(holder),
                    None => holder_of(self.executor(), &collection, object.uid.as_str())
                        .await
                        .ok()
                        .flatten(),
                };
                Ok(ImportedEvent::existing(holder))
            }
            400 | 403 | 412 | 413 | 415 => Err(refusal(response.status)),
            status => Err(CalDavError::status(status, response.body)
                .with_retry_after(response.retry_after)
                .after_write()
                .content_free("importing an event")),
        }
    }
}

/// The calendar's import properties: its largest object and its contents marker.
async fn properties(
    exec: &dyn DavExecutor,
    calendar: &CalendarId,
) -> Result<MultiStatus, CalDavError> {
    exec.send(
        DavMethod::Propfind,
        &collection_href(calendar.as_str())?,
        "0",
        CALENDAR_IMPORT_PROPFIND.to_owned(),
    )
    .await?
    .into_multistatus()
}

/// What the calendar states of the objects it takes.
fn limits_of(props: &MultiStatus) -> CalendarImportLimits {
    let max = props
        .responses
        .first()
        .and_then(|response| response.props.get("max-resource-size"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .and_then(NonZeroU64::new);
    match max {
        Some(max) => CalendarImportLimits::new().with_max_event_size(max),
        None => CalendarImportLimits::new(),
    }
}

/// The object in `collection` whose `UID` is `uid`, if there is one.
///
/// A server that refuses the query (`calendar-query` is RFC 4791's, but a `4xx` here says only
/// that it was not answered) is not asked again: the `PUT` goes ahead, and the server's own
/// refusal of a second `UID` is what remains. Any other failure answers the event.
async fn holder_of(
    exec: &dyn DavExecutor,
    collection: &str,
    uid: &str,
) -> ProviderResult<Option<EventId>> {
    let response = exec
        .send(DavMethod::Report, collection, "1", uid_query(uid))
        .await
        .map_err(|err| err.content_free("looking up an event's UID"))?;
    if (400..500).contains(&response.status) && !matches!(response.status, 401 | 429) {
        return Ok(None);
    }
    let found = response
        .into_multistatus()
        .map_err(|err| err.content_free("looking up an event's UID"))?;
    Ok(found.responses.iter().find_map(|object| {
        let data = object.props.get("calendar-data")?;
        let held = import_object(data).ok()?;
        (held.uid.as_str() == uid)
            .then(|| EventId::try_from(object.href()).ok())
            .flatten()
    }))
}

/// Whether a `412`'s body names one of the CalDAV preconditions an object is refused by.
fn names_an_object_precondition(body: &str) -> bool {
    OBJECT_PRECONDITIONS
        .iter()
        .any(|name| has_precondition(body, name))
}

#[cfg(test)]
#[path = "calendar_import_tests.rs"]
mod tests;
