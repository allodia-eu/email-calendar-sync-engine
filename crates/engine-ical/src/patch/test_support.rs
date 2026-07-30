//! The fixture and the assertion the patcher's tests are built on.
//!
//! The assertion that matters is not "the new value is in the document" — that passes for
//! a patcher that silently deleted the `RRULE` on its way. It is [`assert_only_changed`]:
//! after the patch, **every logical line the patch did not target is byte-identical**,
//! folding and terminators included. A patcher without that test is a data-loss bug with a
//! green suite.
//!
//! Used by [`patch_tests`](super::patch_tests) (what a patch changes) and
//! [`guard_tests`](super::guard_tests) (what it refuses).

use engine_core::{
    ids::{CalendarId, EventId},
    raw::RawIcal,
    time::{CalendarDateTime, TimeZoneId, UtcDateTime},
};

use super::{
    super::{lines::Document, parse_calendar_object},
    *,
};

/// A resource as a real server hands it back: a zoned weekly series with an `EXDATE`,
/// three attendees (one folded across physical lines, with a `DQUOTE`-quoted `CN`
/// containing a comma), a long folded non-ASCII `DESCRIPTION`, `X-` properties nothing
/// in the projection models, an embedded `VTIMEZONE`, and a `VALARM` — whose own
/// `SUMMARY` and `DESCRIPTION` must never be mistaken for the event's.
pub(super) const SERIES: &str = concat!(
    "BEGIN:VCALENDAR\r\n",
    "VERSION:2.0\r\n",
    "PRODID:-//Example Corp//CalDAV Client//NL\r\n",
    "CALSCALE:GREGORIAN\r\n",
    "BEGIN:VTIMEZONE\r\n",
    "TZID:Europe/Amsterdam\r\n",
    "BEGIN:DAYLIGHT\r\n",
    "TZOFFSETFROM:+0100\r\n",
    "TZOFFSETTO:+0200\r\n",
    "TZNAME:CEST\r\n",
    "DTSTART:19700329T020000\r\n",
    "RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\n",
    "END:DAYLIGHT\r\n",
    "BEGIN:STANDARD\r\n",
    "TZOFFSETFROM:+0200\r\n",
    "TZOFFSETTO:+0100\r\n",
    "TZNAME:CET\r\n",
    "DTSTART:19701025T030000\r\n",
    "RRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\n",
    "END:STANDARD\r\n",
    "END:VTIMEZONE\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:standup-4711@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "CREATED:20251201T093000Z\r\n",
    "LAST-MODIFIED:20260101T080000Z\r\n",
    "SEQUENCE:3\r\n",
    "DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n",
    "DTEND;TZID=Europe/Amsterdam:20260105T100000\r\n",
    "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=20\r\n",
    "EXDATE;TZID=Europe/Amsterdam:20260119T093000\r\n",
    "SUMMARY:Sprintplanning — Zürich\r\n",
    "LOCATION:Vergaderzaal 3\\, tweede verdieping\r\n",
    "DESCRIPTION:Wekelijkse sprintplanning met het hele team. Neem je notities m\r\n",
    " ee\\, en denk aan de retrospectieve — we sluiten af met de demo.\r\n",
    "CLASS:PRIVATE\r\n",
    "TRANSP:OPAQUE\r\n",
    "CATEGORIES:WERK,SPRINT\r\n",
    "X-MICROSOFT-CDO-BUSYSTATUS:BUSY\r\n",
    "X-EXAMPLE-ROOM-BOOKING:room-3;confirmed\r\n",
    "ORGANIZER;CN=Baas:mailto:baas@example.com\r\n",
    "ATTENDEE;CN=Baas;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:baas@example.com\r\n",
    "ATTENDEE;CN=\"van der Berg, Jan\";ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RS\r\n",
    " VP=TRUE:mailto:jan@example.com\r\n",
    "ATTENDEE;CN=Ik;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION:mailto:ik@exampl\r\n",
    " e.com\r\n",
    "BEGIN:VALARM\r\n",
    "ACTION:DISPLAY\r\n",
    "TRIGGER:-PT15M\r\n",
    "SUMMARY:Herinnering\r\n",
    "DESCRIPTION:Sprintplanning begint zo\r\n",
    "END:VALARM\r\n",
    "END:VEVENT\r\n",
    "END:VCALENDAR\r\n",
);

pub(super) fn stamp() -> UtcDateTime {
    UtcDateTime::new(2026, 2, 10, 11, 30, 0).unwrap()
}

pub(super) fn patch() -> EventPatch {
    EventPatch::new(stamp())
}

pub(super) fn amsterdam(local: &str) -> CalendarDateTime {
    CalendarDateTime::Zoned {
        local: local.parse().unwrap(),
        zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
    }
}

pub(super) fn apply(ical: &str, target: &PatchTarget, patch: &EventPatch) -> String {
    patch_event_ical(&RawIcal::new(ical), target, patch)
        .unwrap()
        .as_str()
        .to_owned()
}

/// Asserts that `after` differs from `before` in **only** the logical lines whose
/// property name is in `changed` — every other byte, including the original folding and
/// line terminators, is identical.
///
/// This is the whole point of a structural patcher, so it is checked structurally:
/// strike the named properties out of both documents and what remains must be equal.
pub(super) fn assert_only_changed(before: &str, after: &str, changed: &[&str]) {
    let strike = |text: &str| {
        let doc = Document::parse(text);
        (0..doc.len())
            .filter(|&group| {
                let logical = doc.logical(group);
                let name = super::vevent::property_name(&logical);
                !changed
                    .iter()
                    .any(|target| name.eq_ignore_ascii_case(target))
            })
            .map(|group| doc.render_range(group..group + 1, &super::super::lines::Edits::new()))
            .collect::<String>()
    };
    assert_eq!(
        strike(before),
        strike(after),
        "a line outside {changed:?} changed"
    );
}

pub(super) fn reparse(text: &str) -> engine_core::calendar::Event {
    parse_calendar_object(
        text,
        EventId::try_from("/cal/standup.ics").unwrap(),
        CalendarId::try_from("/cal/").unwrap(),
    )
    .unwrap()
}
