//! Creating a calendar over CalDAV: `MKCALENDAR` (RFC 4791 §5.3.1), then the new collection read
//! back as the listing reads one.
//!
//! - **The client names the collection, and names it afresh each time.** The name a person sees is
//!   the `displayname`; the path segment is the client's to choose, and it is a fresh unique token,
//!   so creating "Work" twice makes two calendars, as on every other target. A create whose answer
//!   was lost is "may be stored" (`CalDavError::after_write`), found in the list.
//! - **Every property goes in the one request.** RFC 4791 makes `MKCALENDAR` all or nothing: a
//!   property the server cannot set fails the whole request. Stalwart, SabreDAV and Nextcloud each
//!   set the name, the description and the Apple `calendar-color` (surveyed live).
//! - **The answer is the calendar as the server holds it**, read back with the listing's own
//!   `PROPFIND`, so a value the server rewrote or dropped shows as it is stored.

use std::hash::{BuildHasher, Hasher};

use async_trait::async_trait;
use engine_core::{calendar::Calendar, ids::AccountId};
use engine_provider::{CalendarCollections, NewCalendar, ProviderError, ProviderResult};

use crate::{
    CalDavProvider,
    calendar::calendar_from_response,
    request::{CALENDAR_LIST_PROPFIND, mkcalendar},
    transport::{DavMethod, Precondition, WriteRequest},
};

#[async_trait]
impl CalendarCollections for CalDavProvider {
    async fn create_calendar(
        &self,
        _account: &AccountId,
        new: &NewCalendar,
    ) -> ProviderResult<Calendar> {
        let href = format!("{}{}/", self.home(), fresh_segment());
        self.executor()
            .send_write(WriteRequest {
                method: DavMethod::Mkcalendar,
                href: href.clone(),
                content_type: Some("application/xml; charset=utf-8"),
                precondition: Precondition::None,
                body: mkcalendar(&new.name, new.description.as_deref(), new.color.as_deref()),
            })
            .await
            .and_then(crate::response::HttpResponse::into_write_etag)
            .map_err(|err| err.content_free("creating a calendar"))?;
        // Created: a failure from here on must not read as "not created", or a retry makes a
        // second calendar.
        let created = || {
            ProviderError::needs_confirmation(
                "creating a calendar: it was created and could not be read back; find it in the \
                 calendar list",
            )
        };
        let listed = self
            .executor()
            .send(
                DavMethod::Propfind,
                &href,
                "0",
                CALENDAR_LIST_PROPFIND.to_owned(),
            )
            .await
            .and_then(crate::response::HttpResponse::into_multistatus)
            .map_err(|_| created())?;
        listed
            .responses
            .iter()
            .find(|response| response.props.is_calendar())
            .and_then(|response| calendar_from_response(response).ok())
            .ok_or_else(created)
    }
}

/// A path segment no earlier create chose (a calendar's or an address book's): 128 bits from two
/// hashers the standard library seeds from the operating system's randomness, mixed with the clock.
/// Unique, not secret, and free of anything a host said.
pub(crate) fn fresh_segment() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let draw = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(salt);
        hasher.finish()
    };
    format!("{:016x}{:016x}", draw(1), draw(2))
}

#[cfg(test)]
#[path = "calendar_create_tests.rs"]
mod tests;
