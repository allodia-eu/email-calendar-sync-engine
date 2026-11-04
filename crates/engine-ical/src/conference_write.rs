//! Writing a meeting made elsewhere into an iCalendar event: an RFC 7986 `CONFERENCE` property
//! for each way in, our block in the notes, and the join link as the location when the event
//! has none. Outlook, Google Calendar and Thunderbird read no `CONFERENCE`, so the block and the
//! location are what an invitee on one of them sees (`calendar-semantics.md`, "Writing a
//! meeting").

use engine_core::calendar::{Conference, ConferenceUri, with_text_block};
use engine_provider::{AttachedConference, NewConference};

use crate::{error::IcalError, scheduling_write::escape_parameter, unfold::unescape_text};

/// The properties that name a meeting: RFC 7986's own, and the vendor ones Google's and
/// Microsoft's invitations carry, which the reader also takes a meeting from.
pub(crate) const MEETING_PROPERTIES: [&str; 3] = [
    "CONFERENCE",
    "X-GOOGLE-CONFERENCE",
    "X-MICROSOFT-SKYPETEAMSMEETINGURL",
];

/// A meeting ready to be written on a create: the notes with its block, its join link, and its
/// `CONFERENCE` lines, unfolded.
pub(crate) struct Attachment {
    pub(crate) notes: String,
    pub(crate) join: String,
    pub(crate) lines: Vec<String>,
}

/// What putting `new` on a new event whose notes are `notes` writes.
///
/// # Errors
///
/// [`IcalError`] for a [`NewConference::Native`], and for a meeting without a web link.
pub(crate) fn attachment(notes: &str, new: &NewConference) -> Result<Attachment, IcalError> {
    let attached = attached(new)?;
    let conference = &attached.conference;
    let join = conference
        .join()
        .ok_or_else(|| IcalError::new("a meeting to attach needs a web link to join it at"))?;
    let block = conference
        .text_block(&attached.text)
        .unwrap_or_else(|| unreachable!("a meeting with a join link has a block"));
    Ok(Attachment {
        notes: with_text_block(notes, &block),
        join: join.as_str().to_owned(),
        lines: conference_lines(conference),
    })
}

/// The meeting a [`NewConference::Attach`] puts on.
///
/// # Errors
///
/// [`IcalError`] for a [`NewConference::Native`], since no iCalendar server makes a meeting of
/// its own.
pub(crate) fn attached(new: &NewConference) -> Result<&AttachedConference, IcalError> {
    match new {
        NewConference::Native(service) => Err(IcalError::new(format!(
            "this calendar's server makes no meetings of its own, so it cannot make a \
             {service:?} one; attach a meeting made elsewhere"
        ))),
        NewConference::Attach(attached) => Ok(attached),
    }
}

/// One `CONFERENCE` line per way in ([`Conference::labelled_ways_in`]).
///
/// A `URI` value is not TEXT, so it is written unescaped (RFC 5545 §3.3.13); a `tel:` keeps its
/// commas as they are.
pub(crate) fn conference_lines(conference: &Conference) -> Vec<String> {
    conference
        .labelled_ways_in()
        .map(|(point, label)| {
            let mut line = "CONFERENCE;VALUE=URI".to_owned();
            let features: Vec<String> = point
                .offered_features()
                .iter()
                .map(|feature| feature.as_str().to_ascii_uppercase())
                .collect();
            if !features.is_empty() {
                line.push_str(";FEATURE=");
                line.push_str(&features.join(","));
            }
            if let Some(label) = label {
                line.push_str(";LABEL=");
                line.push_str(&escape_parameter(label));
            }
            line.push(':');
            line.push_str(point.uri.as_str());
            line
        })
        .collect()
}

/// The meeting URI a meeting property's value names, read the way the reader reads it.
pub(crate) fn property_uri(value: &str) -> Option<ConferenceUri> {
    ConferenceUri::parse(&unescape_text(value)).ok()
}

#[cfg(test)]
mod tests {
    use engine_core::{
        calendar::{
            ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceText, EntryPoint,
        },
        ids::{CalendarId, EventId, Uid},
        time::{CalendarDateTime, UtcDateTime},
    };
    use engine_provider::EventDraft;

    use super::*;
    use crate::{build_event_ical, parse_calendar_object};

    const TALK: &str = "https://cloud.example.com/call/abcd1234";

    fn talk() -> NewConference {
        let mut dial = EntryPoint::at(ConferenceUri::parse("tel:+31201234567,,123456#").unwrap());
        dial.label = Some("+31 20 1234567".into());
        NewConference::attach(
            Conference {
                key: ConferenceKey::new(TALK),
                service: Some(ConferenceService::NextcloudTalk),
                origin: ConferenceOrigin::Attached,
                entry_points: vec![EntryPoint::at(ConferenceUri::parse(TALK).unwrap()), dial],
                label: Some("Nextcloud Talk".into()),
                meeting_id: None,
                notes: None,
            },
            ConferenceText {
                meeting: "Video meeting".into(),
                join: "Join".into(),
                meeting_id: "Meeting ID".into(),
                passcode: "Passcode".into(),
                dial_in: "Dial in".into(),
                dial_in_code: "Conference ID".into(),
            },
        )
    }

    fn draft() -> EventDraft {
        let at =
            |hour| CalendarDateTime::utc(format!("2026-06-25T{hour:02}:00:00").parse().unwrap());
        EventDraft::new(
            CalendarId::try_from("/cal/").unwrap(),
            Uid::new("meeting-1@test.local").unwrap(),
            "Review",
            at(9),
            at(10),
            UtcDateTime::new(2026, 6, 20, 8, 0, 0).unwrap(),
        )
        .description("Bring the figures.")
    }

    fn read(draft: &EventDraft) -> (String, engine_core::calendar::Event) {
        let ical = build_event_ical(draft).unwrap().as_str().to_owned();
        let event = parse_calendar_object(
            &ical,
            EventId::try_from("/cal/m.ics").unwrap(),
            CalendarId::try_from("/cal/").unwrap(),
        )
        .unwrap();
        (ical, event)
    }

    #[test]
    fn a_created_meeting_reads_back_with_its_dial_in_block_and_location() {
        let (ical, event) = read(&draft().conference(talk()));
        // A `tel:` value keeps its commas: a URI is not TEXT.
        assert!(
            ical.replace("\r\n ", "")
                .contains("CONFERENCE;VALUE=URI;FEATURE=PHONE;LABEL=+31 20 1234567:tel:+31201234567,,123456#\r\n"),
            "{ical}"
        );
        assert_eq!(event.conferences.len(), 1, "{:?}", event.conferences);
        let meeting = &event.conferences[0];
        assert_eq!(meeting.origin, ConferenceOrigin::Attached);
        assert_eq!(meeting.service, Some(ConferenceService::NextcloudTalk));
        assert_eq!(meeting.label.as_deref(), Some("Nextcloud Talk"));
        assert_eq!(meeting.entry_points.len(), 2);
        assert_eq!(event.locations[0].name.as_deref(), Some(TALK));
        let description = event.description.as_deref().unwrap();
        assert!(
            description.contains("Dial in: +31 20 1234567"),
            "{description}"
        );
        assert_eq!(event.notes(|_| true).as_deref(), Some("Bring the figures."));
    }

    #[test]
    fn a_created_meeting_leaves_the_persons_location() {
        let (_, event) = read(&draft().location("Room 4").conference(talk()));
        assert_eq!(event.locations.len(), 1);
        assert_eq!(event.locations[0].name.as_deref(), Some("Room 4"));
    }

    #[test]
    fn a_meeting_the_server_would_make_is_refused_on_create() {
        let draft = draft().conference(NewConference::Native(ConferenceService::GoogleMeet));
        let error = build_event_ical(&draft).unwrap_err();
        assert!(
            error.to_string().contains("no meetings of its own"),
            "{error}"
        );
    }
}
