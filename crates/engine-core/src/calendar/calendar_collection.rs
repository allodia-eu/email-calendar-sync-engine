//! Calendar collections.

use serde::{Deserialize, Serialize};

use super::Alert;
use crate::extended::ExtendedProperties;
use crate::ids::CalendarId;
use crate::time::TimeZoneId;
use crate::version::RevisionTokens;

/// The caller's normalized access rights on a calendar.
///
/// Normalizes JMAP `CalendarRights` (RFC 8984 / JMAP Calendars draft), Google
/// `accessRole` (owner/writer/reader/freeBusyReader), and CalDAV privileges onto
/// a small set of booleans. A free-busy-only calendar is visible only for
/// availability, not its events.
// The fields are independent permission flags mirroring the provider rights
// model (JMAP `CalendarRights` is likewise a set of booleans), not a state that
// an enum would express better.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarAccess {
    /// May read the calendar's events.
    pub may_read: bool,
    /// May create, update, and delete events.
    pub may_write: bool,
    /// May share the calendar with others.
    pub may_share: bool,
    /// May delete the calendar itself.
    pub may_delete: bool,
    /// May respond to invitations (RSVP).
    pub may_rsvp: bool,
    /// May read free/busy availability.
    pub may_read_free_busy: bool,
}

impl CalendarAccess {
    /// Full access, as the owner.
    #[must_use]
    pub fn owner() -> Self {
        Self {
            may_read: true,
            may_write: true,
            may_share: true,
            may_delete: true,
            may_rsvp: true,
            may_read_free_busy: true,
        }
    }

    /// Read and RSVP, but not write/share/delete.
    #[must_use]
    pub fn reader() -> Self {
        Self {
            may_read: true,
            may_write: false,
            may_share: false,
            may_delete: false,
            may_rsvp: true,
            may_read_free_busy: true,
        }
    }

    /// Free/busy availability only; events are not readable.
    #[must_use]
    pub fn free_busy_only() -> Self {
        Self {
            may_read: false,
            may_write: false,
            may_share: false,
            may_delete: false,
            may_rsvp: false,
            may_read_free_busy: true,
        }
    }
}

impl Default for CalendarAccess {
    fn default() -> Self {
        Self::owner()
    }
}

/// A calendar collection (JSCalendar/JMAP `Calendar`, CalDAV calendar
/// collection, Google `calendarList` entry).
///
/// A calendar carries access rights, subscription/visibility, owner, default
/// reminders, timezone, and color — not only event membership (`modeling.md`).
/// Default reminders are split into timed and all-day sets, as JMAP requires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    /// The collection's stable id.
    pub id: CalendarId,
    /// The display name.
    pub name: String,
    /// A free-text description.
    pub description: Option<String>,
    /// A display color (CSS color name or `#hex`).
    pub color: Option<String>,
    /// A provider-supplied sort hint; lower sorts first.
    pub sort_order: u32,
    /// Whether the user is subscribed to (wants to see) this calendar.
    pub is_subscribed: bool,
    /// Whether the calendar is currently shown.
    pub is_visible: bool,
    /// Whether this is the default calendar for new events.
    pub is_default: bool,
    /// The owner's address, if shared with the user by someone.
    pub owner: Option<String>,
    /// The calendar's default timezone, used to resolve floating/all-day values.
    pub time_zone: Option<TimeZoneId>,
    /// Default alerts inherited by timed events that opt in.
    pub default_alerts_with_time: Vec<Alert>,
    /// Default alerts inherited by all-day events that opt in.
    pub default_alerts_without_time: Vec<Alert>,
    /// The caller's access rights.
    pub access: CalendarAccess,
    /// Per-object revision tokens, if the provider supplies any.
    pub revisions: RevisionTokens,
    /// Preserved provider-defined extended properties.
    pub extended: ExtendedProperties,
}

impl Calendar {
    /// Creates an owned, subscribed, visible calendar with the given id and name.
    #[must_use]
    pub fn new(id: CalendarId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            description: None,
            color: None,
            sort_order: 0,
            is_subscribed: true,
            is_visible: true,
            is_default: false,
            owner: None,
            time_zone: None,
            default_alerts_with_time: Vec::new(),
            default_alerts_without_time: Vec::new(),
            access: CalendarAccess::owner(),
            revisions: RevisionTokens::none(),
            extended: ExtendedProperties::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> CalendarId {
        CalendarId::try_from("cal-1").unwrap()
    }

    #[test]
    fn new_calendar_is_owned_and_visible() {
        let cal = Calendar::new(id(), "Work");
        assert!(cal.access.may_write);
        assert!(cal.is_subscribed);
        assert!(cal.is_visible);
        assert!(!cal.is_default);
    }

    #[test]
    fn access_levels_differ() {
        assert!(!CalendarAccess::reader().may_write);
        assert!(CalendarAccess::reader().may_rsvp);
        assert!(!CalendarAccess::free_busy_only().may_read);
        assert!(CalendarAccess::free_busy_only().may_read_free_busy);
    }

    #[test]
    fn roundtrips_through_json() {
        let mut cal = Calendar::new(id(), "Shared");
        cal.access = CalendarAccess::reader();
        cal.owner = Some("boss@example.com".into());
        cal.time_zone = Some(TimeZoneId::iana("Europe/Amsterdam").unwrap());
        let json = serde_json::to_string(&cal).unwrap();
        assert_eq!(serde_json::from_str::<Calendar>(&json).unwrap(), cal);
    }
}
