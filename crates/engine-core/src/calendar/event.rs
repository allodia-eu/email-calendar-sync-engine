//! The normalized calendar event.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{Alert, EventKind, Location, Participant, Recurrence, VirtualLocation};
use crate::extended::ExtendedProperties;
use crate::ids::{CalendarId, EventId, Uid};
use crate::membership::Memberships;
use crate::raw::{RawIcal, RawJsCalendar};
use crate::time::{CalendarDateTime, Duration, UtcDateTime};
use crate::version::RevisionTokens;

open_enum! {
    /// An event's status (JSCalendar `status`, RFC 8984 §5.1.3; iCalendar
    /// `STATUS`). `cancelled` is a tombstone.
    EventStatus {
        /// Confirmed (the default).
        Confirmed => "confirmed",
        /// Cancelled.
        Cancelled => "cancelled",
        /// Tentatively scheduled.
        Tentative => "tentative",
    }
}

open_enum! {
    /// Whether an event blocks time (JSCalendar `freeBusyStatus`, RFC 8984
    /// §4.4.2; iCalendar `TRANSP`). Defaults to `busy`.
    FreeBusyStatus {
        /// Does not block time.
        Free => "free",
        /// Blocks time (the default).
        Busy => "busy",
    }
}

open_enum! {
    /// An event's privacy level (JSCalendar `privacy`, RFC 8984 §4.4.3;
    /// iCalendar `CLASS`). Defaults to `public`.
    Privacy {
        /// Visible to anyone who can see the calendar.
        Public => "public",
        /// Only limited details may be shared.
        Private => "private",
        /// Invisible to others.
        Secret => "secret",
    }
}

/// A normalized calendar event (JSCalendar `Event`-shaped, RFC 8984 §5.1).
///
/// Identity is the provider object key [`EventId`]; the cross-system [`Uid`] is
/// separate (every recurrence instance shares one `uid` but has a distinct
/// provider id). Membership in calendars is a non-empty set. The scheduled
/// `start` is a [`CalendarDateTime`] and the event end is always
/// `start + duration` — there is no stored end instant. A `recurrence_id` marks
/// this object as a single overridden instance, in which case `recurrence` is
/// absent (RFC 8984 §4.3.1).
///
/// Provider-native payloads ([`RawIcal`]/[`RawJsCalendar`]) and the
/// kind-specific payload (in `extended`) are preserved beside this projection,
/// which exists for display, search, and engine logic and is **not**
/// round-trip-authoritative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// The provider object id.
    pub id: EventId,
    /// The cross-system event identity (iCalendar/JSCalendar `UID`).
    pub uid: Uid,
    /// The calendars this event belongs to (always at least one).
    pub calendars: Memberships<CalendarId>,
    /// The event kind; the kind-specific payload lives in `extended`.
    pub kind: EventKind,
    /// The title (JSCalendar `title`; may be empty).
    pub title: String,
    /// A free-text description.
    pub description: Option<String>,
    /// The scheduled start.
    pub start: CalendarDateTime,
    /// The duration; the end is `start + duration`.
    pub duration: Duration,
    /// The status (`cancelled` is a tombstone).
    pub status: EventStatus,
    /// Whether the event blocks time.
    pub free_busy_status: FreeBusyStatus,
    /// The privacy level.
    pub privacy: Privacy,
    /// The iTIP revision counter (`SEQUENCE`).
    pub sequence: u32,
    /// Priority 0–9 (0 = undefined, 1 = highest).
    pub priority: u8,
    /// The recurrence specification, if this is a recurring master.
    pub recurrence: Option<Recurrence>,
    /// The recurrence id, if this object is a single overridden instance.
    pub recurrence_id: Option<CalendarDateTime>,
    /// The participants.
    pub participants: Vec<Participant>,
    /// Physical locations.
    pub locations: Vec<Location>,
    /// Virtual locations (conference links).
    pub virtual_locations: Vec<VirtualLocation>,
    /// Alerts/reminders.
    pub alerts: Vec<Alert>,
    /// Whether the calendar's default alerts apply instead of `alerts`.
    pub use_default_alerts: bool,
    /// Free-form keywords (JSCalendar `keywords`).
    pub keywords: BTreeSet<String>,
    /// Categories (JSCalendar `categories`, URIs).
    pub categories: BTreeSet<String>,
    /// A display color (CSS color name or `#hex`).
    pub color: Option<String>,
    /// When the event was created.
    pub created: Option<UtcDateTime>,
    /// When the event was last modified.
    pub updated: Option<UtcDateTime>,
    /// Per-object revision tokens, if the provider supplies any.
    pub revisions: RevisionTokens,
    /// The preserved raw iCalendar, if this event came from iCalendar/CalDAV.
    pub raw_ical: Option<RawIcal>,
    /// The preserved raw JSCalendar, if this event came from JSCalendar/JMAP.
    pub raw_jscalendar: Option<RawJsCalendar>,
    /// Preserved provider-defined extended properties and kind-specific payload.
    pub extended: ExtendedProperties,
}

impl Event {
    /// Creates an event with the given identity, calendar membership, and start,
    /// with default metadata and a zero duration.
    #[must_use]
    pub fn new(
        id: EventId,
        uid: Uid,
        calendars: Memberships<CalendarId>,
        start: CalendarDateTime,
    ) -> Self {
        Self {
            id,
            uid,
            calendars,
            kind: EventKind::Default,
            title: String::new(),
            description: None,
            start,
            duration: Duration::ZERO,
            status: EventStatus::Confirmed,
            free_busy_status: FreeBusyStatus::Busy,
            privacy: Privacy::Public,
            sequence: 0,
            priority: 0,
            recurrence: None,
            recurrence_id: None,
            participants: Vec::new(),
            locations: Vec::new(),
            virtual_locations: Vec::new(),
            alerts: Vec::new(),
            use_default_alerts: false,
            keywords: BTreeSet::new(),
            categories: BTreeSet::new(),
            color: None,
            created: None,
            updated: None,
            revisions: RevisionTokens::none(),
            raw_ical: None,
            raw_jscalendar: None,
            extended: ExtendedProperties::new(),
        }
    }

    /// Returns `true` if this event is a recurring master (has at least one
    /// recurrence rule).
    #[must_use]
    pub fn is_recurring(&self) -> bool {
        self.recurrence
            .as_ref()
            .is_some_and(|rec| !rec.rules.is_empty())
    }

    /// Returns `true` if this object is a single overridden instance of a series.
    #[must_use]
    pub fn is_override_instance(&self) -> bool {
        self.recurrence_id.is_some()
    }

    /// Returns `true` if this is an all-day event.
    #[must_use]
    pub fn is_all_day(&self) -> bool {
        self.start.is_all_day()
    }

    /// Returns `true` if the event is cancelled (a tombstone).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.status == EventStatus::Cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{Frequency, RecurrenceRule};
    use crate::time::LocalDateTime;

    fn event() -> Event {
        Event::new(
            EventId::try_from("evt-1").unwrap(),
            Uid::new("uid-1").unwrap(),
            Memberships::of_one(CalendarId::try_from("cal-1").unwrap()),
            CalendarDateTime::Zoned {
                local: LocalDateTime::new(2021, 6, 1, 9, 0, 0).unwrap(),
                zone: crate::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            },
        )
    }

    #[test]
    fn new_event_defaults() {
        let ev = event();
        assert_eq!(ev.status, EventStatus::Confirmed);
        assert_eq!(ev.free_busy_status, FreeBusyStatus::Busy);
        assert_eq!(ev.privacy, Privacy::Public);
        assert_eq!(ev.duration, Duration::ZERO);
        assert!(!ev.is_recurring());
        assert!(!ev.is_override_instance());
        assert!(!ev.is_all_day());
    }

    #[test]
    fn recurring_master_is_detected() {
        let mut ev = event();
        ev.recurrence = Some(Recurrence::from_rule(RecurrenceRule::new(
            Frequency::Weekly,
        )));
        assert!(ev.is_recurring());
    }

    #[test]
    fn event_with_uid_distinct_from_id() {
        let ev = event();
        // The provider object id and the cross-system uid are different things.
        assert_eq!(ev.id.as_str(), "evt-1");
        assert_eq!(ev.uid.as_str(), "uid-1");
    }

    #[test]
    fn roundtrips_through_json() {
        let mut ev = event();
        ev.title = "Standup".into();
        ev.duration = "PT30M".parse().unwrap();
        ev.participants.push(Participant::attendee("a@example.com"));
        ev.raw_ical = Some(RawIcal::new("BEGIN:VEVENT\r\nUID:uid-1\r\nEND:VEVENT"));
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }
}
