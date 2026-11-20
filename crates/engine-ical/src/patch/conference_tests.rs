//! What a conference edit writes into a stored event, read back through the parser.

use engine_core::{
    calendar::{
        Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceText,
        ConferenceUri, EntryPoint,
    },
    raw::RawIcal,
};
use engine_provider::{ConferenceEdit, NewConference, Occurrence};

use super::{test_support::*, *};

const JITSI: &str = "https://meet.example.org/planning-4711";
const TALK: &str = "https://cloud.example.com/call/abcd1234";

/// A one-off event that already carries a Nextcloud Talk meeting written the way this crate
/// writes one: a `CONFERENCE` line for the link and one for its dial-in, our block in the
/// notes, and the link as the location.
const WITH_TALK: &str = concat!(
    "BEGIN:VCALENDAR\r\n",
    "VERSION:2.0\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:talk-1@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "DTSTART:20260105T093000Z\r\n",
    "DTEND:20260105T100000Z\r\n",
    "SUMMARY:Review\r\n",
    "DESCRIPTION:Bring the figures.\\n\\n-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=\\nNextcl\r\n",
    " oud Talk\\nJoin: https://cloud.example.com/call/abcd1234\\nDial in: +31 20 123\r\n",
    " 4567\\n-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=-=\r\n",
    "LOCATION:https://cloud.example.com/call/abcd1234\r\n",
    "CONFERENCE;VALUE=URI;FEATURE=AUDIO,VIDEO;LABEL=Nextcloud Talk:https://cloud.examp\r\n",
    " le.com/call/abcd1234\r\n",
    "CONFERENCE;VALUE=URI;FEATURE=PHONE;LABEL=+31 20 1234567:tel:+31201234567\r\n",
    "X-EXAMPLE-KEEP:yes\r\n",
    "END:VEVENT\r\n",
    "END:VCALENDAR\r\n",
);

/// A one-off event with neither notes nor a location.
const BARE: &str = concat!(
    "BEGIN:VCALENDAR\r\n",
    "VERSION:2.0\r\n",
    "BEGIN:VEVENT\r\n",
    "UID:bare-1@example.com\r\n",
    "DTSTAMP:20260101T080000Z\r\n",
    "DTSTART:20260105T093000Z\r\n",
    "DTEND:20260105T100000Z\r\n",
    "SUMMARY:Review\r\n",
    "END:VEVENT\r\n",
    "END:VCALENDAR\r\n",
);

fn words() -> ConferenceText {
    ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
        dial_in_code: "Conference ID".into(),
    }
}

fn jitsi(label: Option<&str>) -> NewConference {
    NewConference::attach(
        Conference {
            key: ConferenceKey::new(JITSI),
            service: Some(ConferenceService::Jitsi),
            origin: ConferenceOrigin::Attached,
            entry_points: vec![EntryPoint::at(ConferenceUri::parse(JITSI).unwrap())],
            label: label.map(str::to_owned),
            meeting_id: None,
            notes: None,
        },
        words(),
    )
}

fn attach(new: NewConference) -> ConferenceEdit {
    ConferenceEdit::Set {
        replacing: None,
        new,
    }
}

fn unfolded(text: &str) -> String {
    text.replace("\r\n ", "")
}

fn notes(text: &str) -> Option<String> {
    reparse(text)
        .notes(|_| true)
        .map(std::borrow::Cow::into_owned)
}

#[test]
fn attaching_a_meeting_writes_its_line_and_block_and_keeps_the_location() {
    let after = apply(
        SERIES,
        &PatchTarget::Series,
        &patch().conference(attach(jitsi(Some("Jitsi Meet")))),
    );
    assert_only_changed(
        SERIES,
        &after,
        &[
            "DESCRIPTION",
            "CONFERENCE",
            "DTSTAMP",
            "LAST-MODIFIED",
            "SEQUENCE",
        ],
    );
    assert!(unfolded(&after).contains(&format!(
        "\r\nCONFERENCE;VALUE=URI;FEATURE=AUDIO,VIDEO;LABEL=Jitsi Meet:{JITSI}\r\n"
    )));
    let event = reparse(&after);
    assert_eq!(event.conferences.len(), 1, "{:?}", event.conferences);
    assert_eq!(event.conferences[0].origin, ConferenceOrigin::Attached);
    assert_eq!(
        event.conferences[0].join().map(ConferenceUri::as_str),
        Some(JITSI)
    );
    // The person's location stays; the notes read as they did, the block being the meeting's.
    assert_eq!(
        event.locations[0].name.as_deref(),
        Some("Vergaderzaal 3, tweede verdieping")
    );
    assert_eq!(notes(&after), reparse(SERIES).description);
    assert!(
        event
            .description
            .unwrap()
            .contains(&format!("Join: {JITSI}"))
    );
}

#[test]
fn a_meeting_on_an_event_without_a_location_becomes_its_location() {
    let after = apply(
        BARE,
        &PatchTarget::Series,
        &patch().conference(attach(jitsi(None))),
    );
    let event = reparse(&after);
    assert_eq!(event.locations[0].name.as_deref(), Some(JITSI));
    assert_eq!(event.conferences.len(), 1, "{:?}", event.conferences);
    assert_eq!(notes(&after), None);
}

#[test]
fn notes_written_with_the_meeting_are_one_description() {
    let after = apply(
        BARE,
        &PatchTarget::Series,
        &patch()
            .description("Agenda")
            .conference(attach(jitsi(None))),
    );
    assert_eq!(after.matches("\r\nDESCRIPTION").count(), 1, "{after}");
    assert_eq!(notes(&after).as_deref(), Some("Agenda"));
}

#[test]
fn removing_a_meeting_takes_its_lines_block_and_location() {
    let talk = reparse(WITH_TALK);
    assert_eq!(talk.conferences.len(), 1, "{:?}", talk.conferences);
    let key = talk.conferences[0].key.clone();
    assert_eq!(key.as_str(), TALK);

    let after = apply(
        WITH_TALK,
        &PatchTarget::Series,
        &patch().conference(ConferenceEdit::Remove(key)),
    );
    assert!(!after.contains("CONFERENCE"), "{after}");
    assert!(!after.contains("LOCATION"), "{after}");
    assert!(after.contains("X-EXAMPLE-KEEP:yes\r\n"));
    let event = reparse(&after);
    assert!(event.conferences.is_empty(), "{:?}", event.conferences);
    assert_eq!(event.description.as_deref(), Some("Bring the figures."));
}

#[test]
fn a_location_the_edit_sets_survives_the_meetings_removal() {
    let key = reparse(WITH_TALK).conferences[0].key.clone();
    let after = apply(
        WITH_TALK,
        &PatchTarget::Series,
        &patch()
            .location("Room 4")
            .conference(ConferenceEdit::Remove(key)),
    );
    assert_eq!(reparse(&after).locations[0].name.as_deref(), Some("Room 4"));
}

#[test]
fn replacing_a_meeting_swaps_it_for_the_new_one() {
    let key = reparse(WITH_TALK).conferences[0].key.clone();
    let after = apply(
        WITH_TALK,
        &PatchTarget::Series,
        &patch().conference(ConferenceEdit::Set {
            replacing: Some(key),
            new: jitsi(None),
        }),
    );
    let event = reparse(&after);
    assert_eq!(event.conferences.len(), 1, "{:?}", event.conferences);
    assert_eq!(
        event.conferences[0].join().map(ConferenceUri::as_str),
        Some(JITSI)
    );
    // The old link was the location, so the new one takes its place.
    assert_eq!(event.locations[0].name.as_deref(), Some(JITSI));
    assert_eq!(notes(&after).as_deref(), Some("Bring the figures."));
    assert!(!after.contains("abcd1234"), "{after}");
}

#[test]
fn attaching_a_meeting_again_does_not_repeat_it() {
    let once = apply(
        BARE,
        &PatchTarget::Series,
        &patch().conference(attach(jitsi(None))),
    );
    let twice = apply(
        &once,
        &PatchTarget::Series,
        &patch().conference(attach(jitsi(None))),
    );
    assert_eq!(twice.matches("\r\nCONFERENCE").count(), 1, "{twice}");
    assert_eq!(
        reparse(&twice).description,
        reparse(&once).description,
        "one block"
    );
}

#[test]
fn a_label_is_escaped_as_a_parameter_and_reads_back() {
    let label = "Team \"sync\": weekly";
    let after = apply(
        BARE,
        &PatchTarget::Series,
        &patch().conference(attach(jitsi(Some(label)))),
    );
    assert!(
        unfolded(&after).contains(&format!("LABEL=\"Team ^'sync^': weekly\":{JITSI}\r\n")),
        "{after}"
    );
    assert_eq!(reparse(&after).conferences[0].label.as_deref(), Some(label));
}

#[test]
fn a_meeting_the_server_would_make_is_refused() {
    let error = patch_event_ical(
        &RawIcal::new(BARE),
        &PatchTarget::Series,
        &patch().conference(attach(NewConference::Native(ConferenceService::Teams))),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("no meetings of its own"),
        "{error}"
    );
}

#[test]
fn removing_a_meeting_the_event_does_not_hold_is_refused() {
    let error = patch_event_ical(
        &RawIcal::new(BARE),
        &PatchTarget::Series,
        &patch().conference(ConferenceEdit::Remove(ConferenceKey::new(JITSI))),
    )
    .unwrap_err();
    assert!(error.to_string().contains("no meeting"), "{error}");
}

#[test]
fn a_meeting_on_one_occurrence_is_refused() {
    let occurrence = Occurrence::starting(amsterdam("2026-01-12T09:30:00"));
    let error = patch_event_ical(
        &RawIcal::new(SERIES),
        &PatchTarget::Instance(occurrence),
        &patch().conference(attach(jitsi(None))),
    )
    .unwrap_err();
    assert!(error.to_string().contains("whole series"), "{error}");
}
