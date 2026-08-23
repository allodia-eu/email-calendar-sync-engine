//! `engine-ical` — the iCalendar (RFC 5545) layer: text → a normalized [`Event`],
//! the create-path serializer, and the fold-aware line patcher.
//!
//! A calendar object resource is one `VCALENDAR` whose `VEVENT`s all share a `UID`
//! (RFC 4791 §4.1): a series **master** plus its `RECURRENCE-ID` overrides. This
//! crate folds them into a *single* [`Event`] — the master carrying its overrides
//! inline — exactly the shape the JMAP adapter produces from one JSCalendar object,
//! so the recurrence expander and the rest of the engine see one representation
//! regardless of transport. The resource's identity ([`EventId`], from its href) and
//! calendar membership ([`CalendarId`]) are supplied by the caller; the whole
//! resource text is preserved as [`RawIcal`].
//!
//! # Why this is a crate, and not part of the CalDAV adapter
//!
//! iCalendar is not a CalDAV format — it is a *transport-neutral* one, and it arrives
//! over more than one transport. CalDAV carries it as a calendar object resource, but
//! **iMIP carries it over mail, on every account type** (RFC 6047): a Microsoft,
//! Google, or JMAP account receives invitations as a `text/calendar` body part with no
//! CalDAV anywhere in the picture. While the parser lived inside `provider-caldav`, a
//! Gmail- or Graph-only build could not read an invitation at all. So the parser sits
//! beside the model it produces, and `provider-caldav` is one of its callers rather
//! than its owner.
//!
//! There is deliberately **one** iCalendar parser in the engine. It is hardened and
//! fuzzed (`provider_caldav::fuzz_parse`, behind that crate's `fuzzing` feature,
//! drives [`parse_calendar_object`] over arbitrary bytes); a second parser — in a
//! host, in another adapter — would be a second attack surface over the same hostile
//! input, so callers get this one rather than writing their own.

mod build;
mod component;
mod error;
mod event;
mod format;
mod lines;
mod party;
mod patch;
mod recur;
mod recurrence;
mod unfold;
mod value;

// The create-path serializer: how a transport renders the neutral `EventDraft`.
pub use build::build_event_ical;
use component::{Component, parse_components};
use engine_core::{
    calendar::Event,
    ids::{CalendarId, EventId, Uid},
    raw::RawIcal,
    scheduling::{ScheduleMethod, SchedulingMessage},
    time::UtcDateTime,
};
pub use error::IcalError;
use event::{event_from_vevent, vevent_uid};
// The fold-aware line-surgery engine: the one implementation of "rewrite this content
// line, leave every other byte alone", shared by the structural patcher and the CalDAV
// `imip` RSVP primitive.
pub use lines::{Document, Edit, Edits, LineEdit};
// The structural patcher is an *implementation detail* of `Provider::patch_event`: a
// host states the neutral `EventPatch`/`PatchTarget` intent (`engine-provider`) and never
// reaches for the iCalendar surgery itself.
pub use patch::{exclude_occurrence_ical, patch_event_ical};
use recurrence::fold_override;
// The quote-aware splitters are the crate's canonical iCalendar tokenizing
// primitives; the `imip` RSVP patcher reuses them rather than re-implementing.
pub use unfold::{split_once_unquoted, split_unquoted};
use value::parse_utc;

/// Parses one calendar object resource into a single normalized [`Event`].
///
/// The master `VEVENT` (the one without a `RECURRENCE-ID`) becomes the event; its
/// `RECURRENCE-ID` siblings are folded into the event's recurrence overrides. A
/// resource that carries only an override (no master) yields that override as a
/// standalone instance event. `id` and `calendar` come from the resource href and
/// its collection; the full `text` is preserved verbatim as [`RawIcal`].
///
/// # Errors
///
/// Returns [`IcalError`] if the resource has no `VEVENT`, or the master
/// `VEVENT` is missing a `UID`/`DTSTART` or carries an unparseable value.
pub fn parse_calendar_object(
    text: &str,
    id: EventId,
    calendar: CalendarId,
) -> Result<Event, IcalError> {
    let roots = parse_components(text);
    let (components, master_pos) = resource_components(&roots)?;
    let representative = master_pos.unwrap_or(0);
    let mut event =
        event_from_vevent(components[representative], id, calendar, RawIcal::new(text))?;
    fold_overrides(&mut event, &components, master_pos);
    Ok(event)
}

/// Parses an iMIP/iTIP scheduling object (a `text/calendar` body carrying a
/// `METHOD`, RFC 5546/6047) into a normalized [`SchedulingMessage`].
///
/// The `VCALENDAR` `METHOD` and the representative `VEVENT`'s `DTSTAMP` join the
/// folded [`Event`] projection (the same one [`parse_calendar_object`] produces);
/// the event's [`EventId`]/[`CalendarId`] are **synthetic placeholders** derived
/// from its `UID`, since an iMIP body carries no provider href/collection — the
/// real storage identity is assigned when the event is stored, and reconciliation
/// keys on `(UID, SEQUENCE, RECURRENCE-ID)` regardless (`calendar-semantics.md`).
///
/// # Errors
///
/// Returns [`IcalError`] if the object has no `METHOD` (so it is not a
/// scheduling message), no usable `VEVENT`, or a missing/unparseable
/// `UID`/`DTSTART`/`DTSTAMP`.
pub fn parse_scheduling_message(text: &str) -> Result<SchedulingMessage, IcalError> {
    let roots = parse_components(text);
    let method = vcalendar_method(&roots)
        .ok_or_else(|| IcalError::new("scheduling object has no METHOD"))?;
    let (components, master_pos) = resource_components(&roots)?;
    let representative = master_pos.unwrap_or(0);
    let rep = components[representative];
    let (id, calendar) = synthetic_ids(&vevent_uid(rep)?)?;
    let mut event = event_from_vevent(rep, id, calendar, RawIcal::new(text))?;
    fold_overrides(&mut event, &components, master_pos);
    let dtstamp = dtstamp_of(rep)?;
    Ok(SchedulingMessage::new(method, event, dtstamp))
}

/// Collects a resource's same-`UID` `VEVENT`s and the position of the series
/// master (`None` when the resource carries only override instances).
///
/// RFC 4791 §4.1: every component in a resource shares one `UID`. Only that
/// `UID`'s components are kept, so a malformed multi-`UID` resource cannot
/// cross-fold; a sibling whose `UID` cannot be read is skipped, not fatal. The
/// master is the component with no `RECURRENCE-ID` *property* (checked by
/// presence, so a present-but-unparseable `RECURRENCE-ID` is never mistaken for a
/// master).
fn resource_components(roots: &[Component]) -> Result<(Vec<&Component>, Option<usize>), IcalError> {
    let vevents = collect_vevents(roots);
    let first = *vevents
        .first()
        .ok_or_else(|| IcalError::new("resource has no VEVENT"))?;
    let resource_uid = vevent_uid(first)?;
    let components: Vec<&Component> = vevents
        .iter()
        .copied()
        .filter(|vevent| vevent_uid(vevent).is_ok_and(|uid| uid == resource_uid))
        .collect();
    let master_pos = components
        .iter()
        .position(|vevent| vevent.property("RECURRENCE-ID").is_none());
    Ok((components, master_pos))
}

/// Folds the `RECURRENCE-ID` override siblings into `event`, only when a real
/// master anchors the series. Best-effort: a malformed override is skipped, never
/// dropping the master or the rest of the series (`calendar-semantics.md`).
fn fold_overrides(event: &mut Event, components: &[&Component], master_pos: Option<usize>) {
    if let Some(representative) = master_pos {
        for (index, &vevent) in components.iter().enumerate() {
            if index != representative {
                let _ = fold_override(event, vevent);
            }
        }
    }
}

/// Reads the `VCALENDAR` `METHOD` property (case-insensitive), mapping it to a
/// [`ScheduleMethod`]; `None` when no root carries one.
fn vcalendar_method(roots: &[Component]) -> Option<ScheduleMethod> {
    roots
        .iter()
        .find_map(|root| root.value("METHOD"))
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .map(|method| ScheduleMethod::from_wire(&method.to_ascii_lowercase()))
}

/// Reads the representative `VEVENT`'s mandatory iTIP `DTSTAMP` (RFC 5546 §3.2).
fn dtstamp_of(vevent: &Component) -> Result<UtcDateTime, IcalError> {
    let value = vevent
        .value("DTSTAMP")
        .ok_or_else(|| IcalError::new("scheduling VEVENT missing DTSTAMP"))?;
    parse_utc(value)
}

/// Mints the synthetic placeholder ids for a parsed iMIP event from its `UID`
/// (see [`parse_scheduling_message`]).
fn synthetic_ids(uid: &Uid) -> Result<(EventId, CalendarId), IcalError> {
    let id = EventId::try_from(format!("imip:{}", uid.as_str()).as_str())
        .map_err(|e| IcalError::new(format!("bad synthetic event id: {e}")))?;
    let calendar = CalendarId::try_from("imip:scheduling")
        .map_err(|e| IcalError::new(format!("bad synthetic calendar id: {e}")))?;
    Ok((id, calendar))
}

/// Gathers every `VEVENT`, looking inside each `VCALENDAR` but also tolerating a
/// bare top-level `VEVENT`.
fn collect_vevents(roots: &[Component]) -> Vec<&Component> {
    let mut vevents = Vec::new();
    for root in roots {
        if root.name == "VEVENT" {
            vevents.push(root);
        }
        vevents.extend(root.children_named("VEVENT"));
    }
    vevents
}

#[cfg(test)]
mod tests {
    use engine_core::{
        calendar::{FreeBusyStatus, RecurrenceOverride},
        time::{CalendarDateTime, TimeZoneId},
    };

    use super::*;

    fn parse(text: &str) -> Event {
        parse_calendar_object(
            text,
            EventId::try_from("/cal/r.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap()
    }

    const ONE_OFF: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Amsterdam\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:oneoff-2001@test.local\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260318T100000\r\nDTEND;TZID=Europe/Amsterdam:20260318T110000\r\nSUMMARY:One-off zoned event\r\nLOCATION:Amsterdam HQ\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_a_zoned_one_off_event() {
        let event = parse(ONE_OFF);
        assert_eq!(event.uid.as_str(), "oneoff-2001@test.local");
        assert_eq!(event.title, "One-off zoned event");
        assert_eq!(event.duration, "PT1H".parse().unwrap());
        assert_eq!(
            event.start,
            CalendarDateTime::Zoned {
                local: "2026-03-18T10:00:00".parse().unwrap(),
                zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
            }
        );
        assert_eq!(event.locations.len(), 1);
        // The whole resource (including the VTIMEZONE) is preserved verbatim.
        assert!(
            event
                .raw_ical
                .as_ref()
                .unwrap()
                .as_str()
                .contains("VTIMEZONE")
        );
        assert!(!event.is_recurring());
    }

    const RECURRING: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:weekly-2002@test.local\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nDTEND;TZID=Europe/Amsterdam:20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=8\r\nEXDATE;TZID=Europe/Amsterdam:20260119T093000\r\nSUMMARY:Weekly standup\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:weekly-2002@test.local\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:20260126T093000\r\nDTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nDTEND;TZID=Europe/Amsterdam:20260126T143000\r\nSUMMARY:Weekly standup (moved)\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn folds_master_and_recurrence_id_override_into_one_event() {
        let event = parse(RECURRING);
        // One event, the master, carrying the series rule.
        assert!(event.is_recurring());
        assert!(event.recurrence_id.is_none());
        let recurrence = event.recurrence.as_ref().unwrap();
        assert_eq!(recurrence.rules.len(), 1);

        // The EXDATE became an exclusion; the RECURRENCE-ID VEVENT became a patch.
        let excluded: CalendarDateTime = CalendarDateTime::Zoned {
            local: "2026-01-19T09:30:00".parse().unwrap(),
            zone: TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        };
        assert!(recurrence.is_excluded(&excluded.local().unwrap()));
        let moved = "2026-01-26T09:30:00".parse().unwrap();
        assert!(matches!(
            recurrence.overrides.get(&moved),
            Some(RecurrenceOverride::Patch(_))
        ));
    }

    #[test]
    fn all_day_event_is_zoneless_and_transparent() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:allday-2005@test.local\r\nDTSTART;VALUE=DATE:20260401\r\nDTEND;VALUE=DATE:20260402\r\nSUMMARY:All-day\r\nTRANSP:TRANSPARENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let event = parse(text);
        assert!(event.is_all_day());
        assert!(event.start.zone().is_none());
        assert_eq!(event.free_busy_status, FreeBusyStatus::Free);
        assert_eq!(event.duration, "P1D".parse().unwrap());
    }

    #[test]
    fn a_malformed_override_does_not_drop_the_whole_series() {
        // A valid master plus an override whose RECURRENCE-ID is unparseable: the
        // master (and its rule) must survive; only the bad override is skipped.
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260105T093000\r\n\
             RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=8\r\nSUMMARY:Standup\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:w@x\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:garbage\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nSUMMARY:Moved\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n";
        let event = parse(text);
        assert_eq!(event.uid.as_str(), "w@x");
        assert_eq!(event.title, "Standup");
        assert!(
            event.is_recurring(),
            "the master's rule survives the bad override"
        );
        assert!(event.recurrence_id.is_none());
    }

    #[test]
    fn a_standalone_malformed_override_resource_still_errors() {
        // With no valid master and the only VEVENT carrying an unparseable
        // RECURRENCE-ID, the resource has nothing usable → an error (skipped by the
        // sync layer), not a panic.
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\n\
             RECURRENCE-ID;TZID=Europe/Amsterdam:garbage\r\n\
             DTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(
            parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn a_resource_without_a_vevent_is_an_error() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:t\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        assert!(
            parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            )
            .is_err()
        );
    }

    // --- iMIP / iTIP scheduling parse (parse_scheduling_message) -------------

    const REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T080000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nDTEND;TZID=Europe/Amsterdam:20260601T093000\r\nSUMMARY:Sprint planning\r\nSEQUENCE:2\r\nORGANIZER;CN=Boss:mailto:boss@test.local\r\nATTENDEE;CN=Boss;ROLE=CHAIR;PARTSTAT=ACCEPTED:mailto:boss@test.local\r\nATTENDEE;CN=Me;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_an_imip_request() {
        use engine_core::scheduling::{ImipTrust, InstanceKey, Revision, ScheduleMethod};

        let msg = parse_scheduling_message(REQUEST).unwrap();
        assert_eq!(msg.method, ScheduleMethod::Request);
        assert_eq!(msg.event.uid.as_str(), "meeting-7@test.local");
        assert_eq!(msg.event.title, "Sprint planning");
        assert_eq!(msg.dtstamp.to_string(), "2026-05-01T08:00:00Z");
        // The METHOD + DTSTAMP + SEQUENCE drive the reconciliation key/revision.
        assert_eq!(
            msg.instance_key(),
            InstanceKey::series(msg.event.uid.clone())
        );
        assert_eq!(
            msg.revision(),
            Revision::new(2, "2026-05-01T08:00:00Z".parse().unwrap())
        );
        assert_eq!(msg.organizer(), Some("boss@test.local"));
        // Trust verifies against the ORGANIZER for a REQUEST.
        assert_eq!(msg.trust(Some("boss@test.local")), ImipTrust::Trusted);
        // The raw is preserved for a round-tripping RSVP.
        assert!(
            msg.event
                .raw_ical
                .as_ref()
                .unwrap()
                .as_str()
                .contains("METHOD:REQUEST")
        );
    }

    #[test]
    fn parses_an_imip_reply_with_partstat() {
        use engine_core::{
            calendar::ParticipationStatus,
            scheduling::{ImipTrust, ScheduleMethod},
        };

        let text = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:meeting-7@test.local\r\nDTSTAMP:20260501T090000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nSEQUENCE:2\r\nORGANIZER:mailto:boss@test.local\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:me@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let msg = parse_scheduling_message(text).unwrap();
        assert_eq!(msg.method, ScheduleMethod::Reply);
        assert_eq!(msg.replying_attendee(), Some("me@test.local"));
        assert_eq!(msg.reply_status(), Some(&ParticipationStatus::Accepted));
        // A REPLY verifies against the replying ATTENDEE, not the organizer.
        assert_eq!(msg.trust(Some("me@test.local")), ImipTrust::Trusted);
        assert!(matches!(
            msg.trust(Some("boss@test.local")),
            ImipTrust::Untrusted(_)
        ));
    }

    #[test]
    fn parses_a_cancel_targeting_one_instance() {
        use engine_core::scheduling::ScheduleMethod;

        let text = "BEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:weekly-9@test.local\r\nDTSTAMP:20260501T100000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260608T093000\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:20260608T093000\r\nSEQUENCE:3\r\nORGANIZER:mailto:boss@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let msg = parse_scheduling_message(text).unwrap();
        assert_eq!(msg.method, ScheduleMethod::Cancel);
        let key = msg.instance_key();
        assert!(!key.is_series(), "a RECURRENCE-ID targets one instance");
    }

    #[test]
    fn a_body_without_a_method_is_not_a_scheduling_message() {
        // The read-path ONE_OFF resource (no METHOD) is a stored object, not an
        // iMIP message.
        assert!(parse_scheduling_message(ONE_OFF).is_err());
    }

    #[test]
    fn a_scheduling_message_without_dtstamp_is_rejected() {
        let text = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x@test.local\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nORGANIZER:mailto:boss@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(parse_scheduling_message(text).is_err());
    }

    #[test]
    fn an_unknown_method_is_preserved_for_surfacing() {
        use engine_core::scheduling::ScheduleMethod;

        let text = "BEGIN:VCALENDAR\r\nMETHOD:X-VENDOR-OP\r\nBEGIN:VEVENT\r\nUID:x@test.local\r\nDTSTAMP:20260501T100000Z\r\nDTSTART;TZID=Europe/Amsterdam:20260601T090000\r\nORGANIZER:mailto:boss@test.local\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let msg = parse_scheduling_message(text).unwrap();
        assert_eq!(msg.method, ScheduleMethod::Other("x-vendor-op".to_owned()));
    }

    #[test]
    fn adversarial_input_does_not_panic() {
        // Truncated, mis-nested, and junk inputs must fail gracefully, never panic.
        for text in [
            "",
            "BEGIN:VCALENDAR",
            "BEGIN:VEVENT\r\nDTSTART:garbage\r\nEND:VEVENT",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:\r\nEND:VEVENT\r\nEND:VCALENDAR",
            ":::::\r\n;;;;;\r\nBEGIN\r\nEND",
        ] {
            let _ = parse_calendar_object(
                text,
                EventId::try_from("/cal/r.ics").unwrap(),
                CalendarId::try_from("/cal/").unwrap(),
            );
        }
    }
}
