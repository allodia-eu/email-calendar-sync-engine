use engine_core::{
    calendar::{
        Conference, ConferenceKey, ConferenceService, ConferenceText, ConferenceUri, EntryPoint,
        Location, NativeState,
    },
    error::FailureClass,
    ids::{CalendarId, EventId, Uid},
    membership::Memberships,
    time::CalendarDateTime,
};

use super::*;
use crate::{ConferenceEdit, EventPatch};

const JITSI: &str = "https://meet.example.org/planning-4711";
const TEAMS: &str = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0";

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

fn meeting(uri: &str, origin: ConferenceOrigin) -> Conference {
    Conference {
        key: ConferenceKey::new(uri),
        service: ConferenceService::recognise(&ConferenceUri::parse(uri).unwrap()),
        origin,
        entry_points: vec![EntryPoint::at(ConferenceUri::parse(uri).unwrap())],
        label: None,
        meeting_id: None,
        notes: None,
    }
}

fn base(notes: &str, location: Option<&str>, conferences: Vec<Conference>) -> Event {
    let mut event = Event::new(
        EventId::try_from("e").unwrap(),
        Uid::new("e@test.local").unwrap(),
        Memberships::of_one(CalendarId::try_from("c").unwrap()),
        CalendarDateTime::utc("2026-09-14T09:00:00".parse().unwrap()),
    );
    event.description = Some(notes.to_owned());
    event.locations = location.into_iter().map(Location::named).collect();
    event.conferences = conferences;
    event
}

fn patch() -> EventPatch {
    EventPatch::new("2026-07-14T10:00:00Z".parse().unwrap())
}

fn attach(conference: Conference) -> ConferenceEdit {
    ConferenceEdit::Set {
        replacing: None,
        new: NewConference::attach(conference, words()),
    }
}

#[test]
fn a_patch_without_a_meeting_edit_leaves_the_text_to_the_patch() {
    let base = base("Notes", None, Vec::new());
    assert_eq!(
        meeting_text(&base, &patch().description("New"), "").unwrap(),
        None
    );
}

#[test]
fn a_location_the_patch_clears_gives_way_to_the_attached_link() {
    let base = base("", Some("Room 4"), Vec::new());
    let text = meeting_text(
        &base,
        &patch()
            .clear_location()
            .conference(attach(meeting(JITSI, ConferenceOrigin::Attached))),
        "Room 4",
    )
    .unwrap()
    .unwrap();
    assert_eq!(text.location, Some(TextEdit::Set(JITSI.to_owned())));
    let Some(TextEdit::Set(notes)) = text.notes else {
        panic!("the block is written")
    };
    assert!(notes.contains(&format!("Join: {JITSI}")), "{notes}");
}

#[test]
fn a_server_made_meetings_stand_in_location_goes_with_it() {
    let teams = meeting(
        TEAMS,
        ConferenceOrigin::Native {
            removable: true,
            state: NativeState::Ready,
        },
    );
    let base = base("Agenda", None, vec![teams.clone()]);
    let text = meeting_text(
        &base,
        &patch().conference(ConferenceEdit::Remove(teams.key)),
        "Microsoft Teams Meeting",
    )
    .unwrap()
    .unwrap();
    assert_eq!(text.location, Some(TextEdit::Clear));
    assert_eq!(
        text.notes, None,
        "no block of the meeting's was in the notes"
    );
}

#[test]
fn a_location_the_person_wrote_is_not_a_stand_in() {
    let jitsi = meeting(JITSI, ConferenceOrigin::Attached);
    let base = base("", Some("Room 4"), vec![jitsi.clone()]);
    let text = meeting_text(
        &base,
        &patch().conference(ConferenceEdit::Remove(jitsi.key)),
        "Room 4",
    )
    .unwrap()
    .unwrap();
    assert_eq!(text.location, None);
}

#[test]
fn removing_a_meeting_the_event_does_not_hold_is_refused() {
    let base = base("", None, Vec::new());
    let error = meeting_text(
        &base,
        &patch().conference(ConferenceEdit::Remove(ConferenceKey::new(JITSI))),
        "",
    )
    .unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
}

#[test]
fn a_meeting_without_a_web_link_is_refused() {
    let mut phone_only = meeting("tel:+31201234567", ConferenceOrigin::Attached);
    phone_only.key = ConferenceKey::new("tel:+31201234567");
    let error = meeting_text(
        &base("", None, Vec::new()),
        &patch().conference(attach(phone_only)),
        "",
    )
    .unwrap_err();
    assert_eq!(error.class(), FailureClass::InvalidState);
}
