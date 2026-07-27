//! Normalizing a CalDAV calendar collection (its WebDAV properties) into the
//! engine's [`Calendar`] container.
//!
//! The collection href is the calendar's stable id (and the membership key its
//! events reference), mirroring how the JMAP adapter uses the JMAP object id. The
//! display name, description, color, and **access rights** come from the PROPFIND
//! props; the remaining richer fields (default reminders, timezone) are left at
//! their defaults for this read slice.

use engine_core::{
    calendar::{Calendar, CalendarAccess},
    ids::CalendarId,
};

use crate::{
    dav::{DavResponse, Props},
    error::CalDavError,
};

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
    calendar.access = access_from_privileges(&response.props);
    Ok(calendar)
}

/// Maps the server's `DAV:current-user-privilege-set` (RFC 3744 §5.4) onto the
/// engine's [`CalendarAccess`].
///
/// The privilege set answers "what may **I** do here", per authenticated principal —
/// which is why it has to be asked, not inferred: a subscribed holiday feed and a
/// colleague's read-only share are ordinary calendar collections whose only
/// distinguishing mark is the privileges they grant *this* user.
///
/// Which privileges count as a write — and why silence means "writable" — is
/// [`Props::grants_member_writes`], shared with the CardDAV address-book path.
///
/// Only `may_write` is derived. The other flags stay at the two presets: the privilege
/// set says nothing standard about whether the *collection itself* may be deleted (that
/// is `DAV:unbind` on the parent home, not on the calendar) or shared, so inventing an
/// answer from one server's spelling would be exactly the over-fit this mapping avoids.
fn access_from_privileges(props: &Props) -> CalendarAccess {
    if props.grants_member_writes() {
        CalendarAccess::owner()
    } else {
        CalendarAccess::reader()
    }
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

    /// The first calendar collection in a captured `multistatus`, mapped.
    fn mapped(xml: &str, href_suffix: &str) -> Calendar {
        let response = parse_multistatus(xml)
            .unwrap()
            .responses
            .into_iter()
            .find(|r| r.props.is_calendar() && r.href().ends_with(href_suffix))
            .expect("the collection is listed as a calendar");
        calendar_from_response(&response).unwrap()
    }

    #[test]
    fn maps_the_seed_default_calendar() {
        let calendar = mapped(
            include_str!("../tests/fixtures/calendar-home.xml"),
            "/default/",
        );
        assert_eq!(calendar.id.as_str(), "/dav/cal/alice%40test.local/default/");
        assert_eq!(calendar.name, "Stalwart Calendar (alice@test.local)");
        // Stalwart grants the owner set on Alice's own calendar.
        assert!(calendar.access.may_write);
    }

    #[test]
    fn a_share_granting_no_write_privilege_is_a_reader() {
        // Bob's calendar, shared with Alice read-only: SabreDAV grants her `read` and
        // `write-properties` (she may rename her copy) but neither `write` nor
        // `write-content`, so no event may be written into it. The old default — every
        // collection an owner — claimed she could.
        let shared = mapped(
            include_str!("../tests/fixtures/calendar-home-sabredav.xml"),
            "/bob-readonly/",
        );
        assert_eq!(shared.name, "Bob (read-only)");
        assert!(!shared.access.may_write);
        assert!(shared.access.may_read, "a reader may still read it");
        assert_eq!(shared.access, CalendarAccess::reader());
    }

    #[test]
    fn the_same_server_reports_the_users_own_calendar_as_writable() {
        // The other half of the pair: one server, one PROPFIND, two honest answers.
        let own = mapped(
            include_str!("../tests/fixtures/calendar-home-sabredav.xml"),
            "/default/",
        );
        assert!(own.access.may_write);
    }

    #[test]
    fn a_server_that_reports_no_privileges_is_taken_as_writable() {
        // Silence is not a "no": RFC 4791 §2 requires ACL support, so a server that
        // says nothing is non-conformant rather than restrictive. Hiding the edit
        // affordance there would be the worse failure; the write's `403` is the
        // backstop.
        let xml = "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/dav/cal/u/work/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
        let calendar = mapped(xml, "/work/");
        assert_eq!(calendar.access, CalendarAccess::owner());
    }

    #[test]
    fn falls_back_to_the_href_segment_for_a_nameless_calendar() {
        let xml = "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:response><D:href>/dav/cal/u/work/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
        let response = &parse_multistatus(xml).unwrap().responses[0];
        let calendar = calendar_from_response(response).unwrap();
        assert_eq!(calendar.name, "work");
    }
}
