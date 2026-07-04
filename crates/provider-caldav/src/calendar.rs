//! Normalizing a CalDAV calendar collection (its WebDAV properties) into the
//! engine's [`Calendar`] container.
//!
//! The collection href is the calendar's stable id (and the membership key its
//! events reference), mirroring how the JMAP adapter uses the JMAP object id. The
//! display name, description, and color come from the PROPFIND props; richer
//! fields (access rights, default reminders, timezone) are left at their defaults
//! for this read slice.

use engine_core::calendar::Calendar;
use engine_core::ids::CalendarId;

use crate::dav::DavResponse;
use crate::error::CalDavError;

/// Maps one calendar-collection response into a [`Calendar`].
///
/// # Errors
///
/// Returns [`CalDavError::Protocol`] if the response carries no usable href.
pub(crate) fn calendar_from_response(response: &DavResponse) -> Result<Calendar, CalDavError> {
    let href = response.href();
    let id = CalendarId::try_from(href)
        .map_err(|e| CalDavError::protocol(format!("bad calendar href {href:?}: {e}")))?;
    let name = response
        .props
        .get("displayname")
        .map_or_else(|| name_from_href(href), str::to_owned);
    let mut calendar = Calendar::new(id, name);
    calendar.description = response
        .props
        .get("calendar-description")
        .map(str::to_owned);
    calendar.color = response.props.get("calendar-color").map(str::to_owned);
    Ok(calendar)
}

/// Derives a display name from the last path segment of a href, when the server
/// supplied no `displayname`.
fn name_from_href(href: &str) -> String {
    href.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(href)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dav::parse_multistatus;

    #[test]
    fn maps_the_seed_default_calendar() {
        let xml = include_str!("../tests/fixtures/calendar-home.xml");
        let response = parse_multistatus(xml)
            .unwrap()
            .responses
            .into_iter()
            .find(|r| r.props.is_calendar())
            .unwrap();
        let calendar = calendar_from_response(&response).unwrap();
        assert_eq!(calendar.id.as_str(), "/dav/cal/alice%40test.local/default/");
        assert_eq!(calendar.name, "Stalwart Calendar (alice@test.local)");
    }

    #[test]
    fn falls_back_to_the_href_segment_for_a_nameless_calendar() {
        let xml = "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/dav/cal/u/work/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
        let response = &parse_multistatus(xml).unwrap().responses[0];
        let calendar = calendar_from_response(response).unwrap();
        assert_eq!(calendar.name, "work");
    }
}
