//! iMIP (iTIP over email, RFC 6047) entry points: parsing an inbound scheduling
//! message, and the outbound RSVP patch.
//!
//! - [`parse`] turns a `text/calendar` body into an
//!   [`engine_core::scheduling::SchedulingMessage`] (delegating to the iCalendar
//!   parser), which the pure `engine_core::scheduling` layer reconciles and trusts.
//! - [`set_my_partstat`] is the **RSVP write primitive** (`calendar-semantics.md`):
//!   it patches *my* `PARTSTAT` into a stored event's raw iCalendar, leaving every
//!   other property byte-for-byte intact, so the result can be `PUT` back under
//!   `If-Match` through the existing `engine_sync::write_calendar_event` outbox
//!   driver. This separates calendar storage (my `PARTSTAT`) from delivery: a CalDAV
//!   auto-schedule server (RFC 6638) sends the iTIP `REPLY` to the organizer when it
//!   sees the changed `PARTSTAT`.
//!   Storage round-trips **from raw plus a targeted patch**, never by re-serializing
//!   the lossy projection (`modeling.md`).
//!
//! Building a standalone iTIP `REPLY` document for **client**-side iMIP delivery
//! over SMTP is deferred with the rest of that path (the SMTP assembler is
//! `text/plain`-only today — `imap-smtp.md`); the documented and wired RSVP path is
//! the conditional `PUT` above.

use core::ops::Range;

use engine_core::calendar::ParticipationStatus;
use engine_core::raw::RawIcal;
use engine_core::scheduling::SchedulingMessage;

use crate::error::CalDavError;
use crate::ical::{split_once_unquoted, split_unquoted};

/// Parses a `text/calendar` iMIP body (a `METHOD` + a `VEVENT`) into a normalized
/// [`SchedulingMessage`] for the pure scheduling layer to reconcile.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the body carries no `METHOD` (so it is not a
/// scheduling message) or has no usable `VEVENT`/`UID`/`DTSTART`/`DTSTAMP`.
pub fn parse(text: &str) -> Result<SchedulingMessage, CalDavError> {
    crate::ical::parse_scheduling_message(text)
}

/// Patches `attendee`'s `PARTSTAT` to `status` in a stored event's raw iCalendar,
/// returning the body to `PUT` back (the RSVP write primitive).
///
/// Only the matching `ATTENDEE` line's `PARTSTAT` parameter changes (an absent one
/// is added); every other line — `X-` properties, `VALARM`s, the other attendees,
/// the organizer — is preserved verbatim, so the round-trip is lossless
/// (`calendar-semantics.md`). The caller supplies a **stored** resource (no
/// transit-only `METHOD`, RFC 4791 §4.1) and the `If-Match` ETag; this function
/// only rewrites the participation status.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the body has no `ATTENDEE` whose calendar
/// address matches `attendee` (you cannot RSVP to an event you are not invited to).
pub fn set_my_partstat(
    ical: &RawIcal,
    attendee: &str,
    status: &ParticipationStatus,
) -> Result<RawIcal, CalDavError> {
    // The engine `ParticipationStatus` carries the JSCalendar spelling (lowercase,
    // e.g. `accepted`); iCalendar `PARTSTAT` values are uppercase (RFC 5545
    // §3.2.12), the exact inverse of how the parser lowercases on read.
    let ical_status = status.as_str().to_ascii_uppercase();
    let patched = rewrite_my_partstat(ical.as_str(), attendee, &ical_status)
        .ok_or_else(|| CalDavError::ical(format!("no ATTENDEE matching {attendee:?} to RSVP")))?;
    Ok(RawIcal::new(patched))
}

/// Rewrites the first `ATTENDEE` line for `me`, returning the patched document or
/// `None` if no such attendee is present.
fn rewrite_my_partstat(raw: &str, me: &str, status: &str) -> Option<String> {
    let physical = physical_lines(raw);
    let groups = logical_groups(&physical);
    let mut out = String::with_capacity(raw.len() + 16);
    let mut patched = false;
    for group in groups {
        let logical = unfold_group(&physical, &group);
        if !patched && is_attendee_for(&logical, me) {
            let term = physical[group.end - 1].1;
            let term = if term.is_empty() { "\r\n" } else { term };
            out.push_str(&fold(&rewrite_partstat_line(&logical, status), term));
            out.push_str(term);
            patched = true;
        } else {
            for &(content, term) in &physical[group] {
                out.push_str(content);
                out.push_str(term);
            }
        }
    }
    patched.then_some(out)
}

/// Splits `raw` into physical lines as `(content_without_terminator, terminator)`,
/// preserving each original `\r\n`/`\n` (or `""` for an unterminated final line) so
/// untouched lines re-emit byte-for-byte.
fn physical_lines(raw: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            let content_end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            out.push((&raw[start..content_end], &raw[content_end..=i]));
            start = i + 1;
        }
    }
    if start < raw.len() {
        out.push((&raw[start..], ""));
    }
    out
}

/// Groups physical lines into logical content lines, attaching each folded
/// continuation (a line beginning with a space or tab, RFC 5545 §3.1) to its
/// predecessor.
fn logical_groups(physical: &[(&str, &str)]) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < physical.len() {
        let start = i;
        i += 1;
        while i < physical.len() && physical[i].0.starts_with([' ', '\t']) {
            i += 1;
        }
        groups.push(start..i);
    }
    groups
}

/// Unfolds the physical lines of one group into a single logical content line,
/// stripping each continuation's one leading space/tab.
fn unfold_group(physical: &[(&str, &str)], group: &Range<usize>) -> String {
    let mut logical = String::new();
    for (offset, &(content, _)) in physical[group.clone()].iter().enumerate() {
        if offset == 0 {
            logical.push_str(content);
        } else {
            logical.push_str(&content[1..]);
        }
    }
    logical
}

/// Returns `true` if `logical` is an `ATTENDEE` property whose calendar address
/// matches `me` (case- and `mailto:`-scheme-insensitive).
fn is_attendee_for(logical: &str, me: &str) -> bool {
    let name_end = logical.find([';', ':']).unwrap_or(logical.len());
    if !logical[..name_end].eq_ignore_ascii_case("ATTENDEE") {
        return false;
    }
    match split_once_unquoted(logical, ':') {
        Some((_, value)) => addresses_match(value, me),
        None => false,
    }
}

/// Rewrites the `PARTSTAT` parameter of an `ATTENDEE` logical line to `status`,
/// preserving every other parameter and the value verbatim. An absent `PARTSTAT`
/// is appended.
fn rewrite_partstat_line(logical: &str, status: &str) -> String {
    let colon = split_once_unquoted(logical, ':').map_or(logical.len(), |(head, _)| head.len());
    let (head, value) = logical.split_at(colon);
    let segments = split_unquoted(head, ';');
    let mut out = String::from(segments[0]); // the property name
    let mut replaced = false;
    for segment in &segments[1..] {
        out.push(';');
        if param_key(segment).eq_ignore_ascii_case("PARTSTAT") {
            out.push_str("PARTSTAT=");
            out.push_str(status);
            replaced = true;
        } else {
            out.push_str(segment);
        }
    }
    if !replaced {
        out.push_str(";PARTSTAT=");
        out.push_str(status);
    }
    out.push_str(value);
    out
}

/// The key of a `KEY=VALUE` parameter segment (trimmed; the whole segment if it has
/// no `=`).
fn param_key(segment: &str) -> &str {
    segment.split('=').next().unwrap_or(segment).trim()
}

/// Folds `line` to ≤75-octet physical lines with `term` + a space (RFC 5545 §3.1),
/// never splitting a multi-byte character. Does not emit a trailing terminator.
fn fold(line: &str, term: &str) -> String {
    const MAX: usize = 75;
    let mut out = String::with_capacity(line.len());
    let mut octets = 0;
    for ch in line.chars() {
        let width = ch.len_utf8();
        if octets + width > MAX {
            out.push_str(term);
            out.push(' ');
            octets = 1; // the continuation's leading space
        }
        out.push(ch);
        octets += width;
    }
    out
}

/// Compares two calendar addresses after lowercasing and stripping a leading
/// `mailto:` (mirrors `engine_core::scheduling`'s internal normalization).
fn addresses_match(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// Lowercases and strips a leading `mailto:` scheme from a calendar address.
fn normalize(address: &str) -> String {
    let lowered = address.trim().to_ascii_lowercase();
    lowered
        .strip_prefix("mailto:")
        .unwrap_or(&lowered)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::ids::{CalendarId, EventId};

    /// A stored (no METHOD) resource where `me` is a needs-action attendee, plus a
    /// VALARM and an X- property the lossy projection cannot express.
    const STORED: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//T//EN\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nX-CUSTOM-FLAG:keep-me\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn ids() -> (EventId, CalendarId) {
        (
            EventId::try_from("/cal/r.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
    }

    #[test]
    fn sets_my_partstat_and_preserves_everything_else() {
        let patched = set_my_partstat(
            &RawIcal::new(STORED),
            "me@test.local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        let text = patched.as_str();

        // The model invariant: properties the projection cannot express survive.
        assert!(text.contains("X-CUSTOM-FLAG:keep-me"));
        assert!(text.contains("BEGIN:VALARM"));
        assert!(text.contains("TRIGGER:-PT15M"));
        // No transit-only METHOD is introduced.
        assert!(!text.contains("METHOD:"));

        // Re-parse: my status is accepted; the organizer's is untouched.
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(text, id, cal).unwrap();
        let me = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("me@test.local"))
            .unwrap();
        assert_eq!(me.participation_status, ParticipationStatus::Accepted);
        // The CN and ROLE parameters on my line survived the rewrite.
        assert_eq!(me.name.as_deref(), Some("Me"));
        let boss = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("boss@test.local"))
            .unwrap();
        assert_eq!(boss.participation_status, ParticipationStatus::Accepted);
    }

    #[test]
    fn declining_changes_only_my_partstat_and_keeps_my_other_params() {
        use engine_core::calendar::ParticipantRole;

        let patched = set_my_partstat(
            &RawIcal::new(STORED),
            "me@test.local",
            &ParticipationStatus::Declined,
        )
        .unwrap();
        // Re-parse (fold-agnostic): my status is now DECLINED, and my other line
        // parameters — CN, ROLE, RSVP — all survived the in-place rewrite. The
        // organizer's status is untouched. No NEEDS-ACTION remains in the body.
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        let me = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("me@test.local"))
            .unwrap();
        assert_eq!(me.participation_status, ParticipationStatus::Declined);
        assert_eq!(me.name.as_deref(), Some("Me"));
        assert!(me.has_role(&ParticipantRole::Attendee)); // REQ-PARTICIPANT
        assert!(me.expect_reply); // RSVP=TRUE
        let boss = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("boss@test.local"))
            .unwrap();
        assert_eq!(boss.participation_status, ParticipationStatus::Accepted);
    }

    #[test]
    fn adds_partstat_when_absent() {
        // An ATTENDEE line carrying no PARTSTAT gets one appended (NEEDS-ACTION is
        // the absent default, RFC 5545 §3.2.12).
        let stored = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nATTENDEE;CN=Me:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let patched = set_my_partstat(
            &RawIcal::new(stored),
            "me@test.local",
            &ParticipationStatus::Tentative,
        )
        .unwrap();
        assert!(patched.as_str().contains("PARTSTAT=TENTATIVE"));
        assert!(patched.as_str().contains("CN=Me"));
    }

    #[test]
    fn matches_attendee_case_and_scheme_insensitively() {
        // A differently-cased, mailto:-prefixed address still finds my line.
        let patched = set_my_partstat(
            &RawIcal::new(STORED),
            "MAILTO:Me@Test.Local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        let me = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("me@test.local"))
            .unwrap();
        assert_eq!(me.participation_status, ParticipationStatus::Accepted);
    }

    #[test]
    fn rsvp_to_an_event_i_am_not_on_is_an_error() {
        let err = set_my_partstat(
            &RawIcal::new(STORED),
            "stranger@example.com",
            &ParticipationStatus::Accepted,
        )
        .unwrap_err();
        assert!(matches!(err, CalDavError::Ical(_)));
    }

    #[test]
    fn folds_a_long_attendee_line_to_compliant_widths() {
        // A long display name forces the rewritten line over 75 octets; it must be
        // folded so every physical line is ≤75 octets (RFC 5545 §3.1).
        let long = "Wilhelmina Aleida Catharina van der Bergen-Vandenbroucke the Third";
        let stored = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nATTENDEE;CN={long};PARTSTAT=NEEDS-ACTION:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let patched = set_my_partstat(
            &RawIcal::new(&stored),
            "me@test.local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        for line in patched.as_str().split("\r\n") {
            assert!(line.len() <= 75, "line over 75 octets: {line:?}");
        }
        // And it still parses back with my accepted status and full name intact.
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        let me = &event.participants[0];
        assert_eq!(me.participation_status, ParticipationStatus::Accepted);
        assert_eq!(me.name.as_deref(), Some(long));
    }

    #[test]
    fn patches_a_folded_attendee_line_with_a_quoted_cn() {
        // My ATTENDEE line is folded across two physical lines and carries a
        // DQUOTE-quoted CN whose value contains a comma. The patch must unfold the
        // line, find me *past* the quoted run (the comma is not a delimiter), set
        // my PARTSTAT, and preserve the quoted name.
        let stored = concat!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\n",
            "DTSTART;TZID=Europe/Amsterdam:20260601T090000\r\n",
            "ATTENDEE;CN=\"van der Berg, Jan\";ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-\r\n",
            " ACTION:mailto:me@test.local\r\n",
            "END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let patched = set_my_partstat(
            &RawIcal::new(stored),
            "me@test.local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        let me = &event.participants[0];
        assert_eq!(me.participation_status, ParticipationStatus::Accepted);
        assert_eq!(me.name.as_deref(), Some("van der Berg, Jan"));
    }

    #[test]
    fn handles_lf_line_endings_and_no_trailing_newline() {
        // A resource using bare LF (not CRLF) with no terminator on its final line:
        // physical_lines must still split each line and re-emit untouched ones.
        let stored = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x@y\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\nATTENDEE;CN=Me:mailto:me@test.local\nEND:VEVENT\nEND:VCALENDAR";
        let patched = set_my_partstat(
            &RawIcal::new(stored),
            "me@test.local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        // The final unterminated line survived, and the patch parses back.
        assert!(patched.as_str().contains("END:VCALENDAR"));
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        assert_eq!(
            event.participants[0].participation_status,
            ParticipationStatus::Accepted
        );
    }

    #[test]
    fn a_malformed_attendee_without_a_value_colon_is_skipped() {
        // A line beginning "ATTENDEE" but with no value colon is not a usable
        // attendee (is_attendee_for → false); it is preserved verbatim while my
        // real attendee is found and patched.
        let stored = concat!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\n",
            "DTSTART;TZID=Europe/Amsterdam:20260601T090000\r\n",
            "ATTENDEE;CN=Ghost\r\n",
            "ATTENDEE;CN=Me:mailto:me@test.local\r\n",
            "END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let patched = set_my_partstat(
            &RawIcal::new(stored),
            "me@test.local",
            &ParticipationStatus::Accepted,
        )
        .unwrap();
        assert!(patched.as_str().contains("ATTENDEE;CN=Ghost"));
        let (id, cal) = ids();
        let event = crate::ical::parse_calendar_object(patched.as_str(), id, cal).unwrap();
        let me = event
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("me@test.local"))
            .unwrap();
        assert_eq!(me.participation_status, ParticipationStatus::Accepted);
    }

    #[test]
    fn parse_delegates_to_the_ical_scheduling_parser() {
        let text = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nORGANIZER:mailto:boss@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let msg = parse(text).unwrap();
        assert_eq!(msg.event.uid.as_str(), "x@y");
        // A body with no METHOD is not a scheduling message.
        assert!(parse("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n").is_err());
    }
}
