//! Importing events into CalDAV calendars (the `import` feature, `providers.md` → "Import").
//!
//! Each event is one calendar object, stored by one `PUT`:
//!
//! - **It tells nobody.** The object goes through [`engine_ical::import_object`], which marks every
//!   `ORGANIZER` and `ATTENDEE`, overrides included, `SCHEDULE-AGENT=CLIENT` (RFC 6638 §7.1), so a
//!   server that schedules sends no invitation, reply or cancellation for it. Proven live against
//!   Stalwart with a control that does send (`tests/scheduling/import.rs`). It is offered only on a
//!   server that advertises `calendar-auto-schedule`, which RFC 6638 binds to honour the parameter
//!   (`CalDavProvider::calendar_importer`).
//! - **A `UID` the calendar holds is already there, never stored twice.** RFC 4791 §5.3.2.1 has the
//!   server refuse a second object with a `UID` (`no-uid-conflict`): Stalwart does with a `403` and
//!   Nextcloud with a `409`, each naming the existing object. SabreDAV's stock backend stores the
//!   duplicate (`docs/spec-deviations.md`, SABREDAV-1), and nothing a server says tells the two
//!   kinds apart, so the `UID` is looked up first (`calendar-query`) on every server. The refusals
//!   stay mapped too, for an object that appears between the lookup and the `PUT`.
//! - **A retry meets its own write.** The object is named after its `UID`
//!   ([`CalDavProvider::event_href`]) and created under `If-None-Match: *`, so a second attempt is
//!   refused `412`; the name is then read, and answered as already there only when the object there
//!   has this `UID`.
//! - **A refusal says whose it is**, by the one write classifier (`CalDavError::write_refusal`):
//!   the object's is `item_unavailable`, the calendar's (full, missing, read-only) is
//!   `ProviderError::refused_by_target`, and a throttle is a throttle.
//! - **The point of no return is the `PUT` going out**: a `PUT` that may have reached the server
//!   and got no answer of its own is "may be stored" (`CalDavError::after_write`).
//!
//! The import cursor is the calendar's `sync-token`, the token a `sync-collection` delta starts
//! from, or its CTag where the server has no report, in the form [`crate::ctag_sync`] reads.
//! Errors carry a class and an HTTP status, never what the server said
//! ([`CalDavError::content_free`]). What a `PUT`, a lookup and the calendar's properties answer is
//! read in `dav_import`, which the card importer shares.

use std::collections::BTreeMap;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, CalendarId, EventId},
    sync::SyncState,
    version::{ETag, RevisionTokens},
};
use engine_ical::{import_object, object_uid};
use engine_provider::{
    ProviderError, ProviderResult,
    import::{CalendarImportLimits, CalendarImporter, EventImport, ImportedEvent},
};

use crate::{
    CalDavProvider,
    dav::MultiStatus,
    dav_import::{PutAnswer, cursor_of, holder, max_size, name_held_by, read_put},
    error::CalDavError,
    href::collection_href,
    request::{CALENDAR_IMPORT_PROPFIND, uid_query},
    transport::{DavExecutor, DavMethod, Precondition, WriteRequest},
};

/// The iCalendar media type an event is stored as.
const ICALENDAR: &str = "text/calendar; charset=utf-8";

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
        cursor_of(&props).ok_or_else(|| {
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
            answers.push(self.import_one(event, &limits[&event.calendar]).await);
        }
        answers
    }
}

impl CalDavProvider {
    /// Imports one event: prepare it, check what will be stored, look its `UID` up, then `PUT` it
    /// where nothing is.
    async fn import_one(
        &self,
        event: &EventImport,
        limits: &CalendarImportLimits,
    ) -> ProviderResult<ImportedEvent> {
        let object = import_object(event.ical.as_str()).map_err(|_| {
            ProviderError::item_unavailable(
                "importing an event: the object is not one event whose parties can be left to \
                 the client",
            )
        })?;
        // The size that counts is the size stored, parameters added included.
        limits.check(&EventImport::new(
            object.ical.clone(),
            event.calendar.clone(),
        ))?;
        let href = self
            .event_href(&event.calendar, &object.uid)
            .map_err(|err| err.content_free("importing an event"))?;
        let collection = collection_href(event.calendar.as_str())
            .map_err(|err| err.content_free("importing an event"))?;
        let uid = object.uid.as_str();
        let holds = |data: &str| object_uid(data).is_ok_and(|held| held.as_str() == uid);
        let held = || {
            holder(
                self.executor(),
                &collection,
                uid_query(uid),
                "calendar-data",
                holds,
            )
        };
        let key = |href: &str| EventId::try_from(href).ok();
        if let Some(existing) = held().await? {
            return Ok(ImportedEvent::existing(key(&existing)));
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
        match read_put(response) {
            PutAnswer::Stored(etag) => Ok(ImportedEvent::created(
                href,
                etag.map_or_else(RevisionTokens::none, |etag| {
                    RevisionTokens::from_etag(ETag::new(etag))
                }),
            )),
            PutAnswer::NameTaken => {
                name_held_by(self.executor(), href.as_str(), holds).await?;
                Ok(ImportedEvent::existing(Some(href)))
            }
            // Stalwart refuses a second `UID` with a `403`, Nextcloud with a `409`, both naming
            // the holder; one that named none is asked for.
            PutAnswer::HeldBy(Some(named)) => Ok(ImportedEvent::existing(key(&named))),
            PutAnswer::HeldBy(None) => Ok(ImportedEvent::existing(
                held().await.ok().flatten().as_deref().and_then(key),
            )),
            PutAnswer::Failed(err) => Err(err.refused_write("importing an event")),
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
    match max_size(props) {
        Some(max) => CalendarImportLimits::new().with_max_event_size(max),
        None => CalendarImportLimits::new(),
    }
}

#[cfg(test)]
#[path = "calendar_import_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "calendar_import_refusal_tests.rs"]
mod refusal_tests;
