use super::*;
use crate::calendar::{
    AccessCodes, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryPoint,
};

fn words() -> ConferenceText {
    ConferenceText {
        meeting: "Video meeting".into(),
        join: "Join".into(),
        meeting_id: "Meeting ID".into(),
        passcode: "Passcode".into(),
        dial_in: "Dial in".into(),
    }
}

fn uri(text: &str) -> ConferenceUri {
    ConferenceUri::parse(text).unwrap()
}

fn jitsi() -> Conference {
    Conference {
        key: ConferenceKey::new("https://meet.example.org/abc123"),
        service: Some(ConferenceService::Jitsi),
        origin: ConferenceOrigin::Attached,
        entry_points: vec![EntryPoint::at(uri("https://meet.example.org/abc123"))],
        label: None,
        meeting_id: None,
        notes: None,
    }
}

#[test]
fn a_block_names_the_meeting_and_its_way_in_between_two_rules() {
    assert_eq!(
        jitsi().text_block(&words()).unwrap(),
        format!("{RULE}\nVideo meeting\nJoin: https://meet.example.org/abc123\n{RULE}")
    );
}

#[test]
fn a_block_carries_the_label_id_passcode_and_numbers_it_has() {
    let mut conference = jitsi();
    conference.label = Some("Jitsi Meet".into());
    conference.meeting_id = Some("123 456 789".into());
    conference.entry_points[0].access = AccessCodes {
        passcode: Some("4321".into()),
        ..AccessCodes::default()
    };
    conference
        .entry_points
        .push(EntryPoint::at(uri("tel:+31-20-000-0000")));
    assert_eq!(
        conference.text_block(&words()).unwrap(),
        format!(
            "{RULE}\nJitsi Meet\nJoin: https://meet.example.org/abc123\nMeeting ID: 123 456 789\n\
             Passcode: 4321\nDial in: +31-20-000-0000\n{RULE}"
        )
    );
}

#[test]
fn a_meeting_without_a_join_link_writes_no_block() {
    let mut conference = jitsi();
    conference.entry_points = vec![EntryPoint::at(uri("tel:+31-20-000-0000"))];
    assert_eq!(conference.text_block(&words()), None);
}

#[test]
fn the_block_goes_after_the_notes_and_comes_out_again_whole() {
    let block = jitsi().text_block(&words()).unwrap();
    let notes = with_text_block("Bring the figures.\n", &block);
    assert_eq!(notes, format!("Bring the figures.\n\n{block}"));
    assert_eq!(with_text_block("  ", &block), block);
    assert_eq!(without_text_block(&notes, &jitsi()), "Bring the figures.");
}

#[test]
fn only_the_block_of_the_meeting_removed_comes_out() {
    let block = jitsi().text_block(&words()).unwrap();
    let mut other = jitsi();
    other.entry_points = vec![EntryPoint::at(uri("https://meet.example.org/other"))];
    let notes = with_text_block("Agenda", &block);
    assert_eq!(without_text_block(&notes, &other), notes);
}

#[test]
fn a_teams_block_comes_out_for_the_teams_meeting_it_joins() {
    let teams = Conference {
        key: ConferenceKey::new("onlineMeeting"),
        service: Some(ConferenceService::Teams),
        origin: ConferenceOrigin::Native {
            removable: true,
            state: crate::calendar::NativeState::Ready,
        },
        entry_points: vec![EntryPoint::at(uri(
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0",
        ))],
        label: None,
        meeting_id: None,
        notes: None,
    };
    let text = "Notes first.\r\nMicrosoft Teams-vergadering\r\n\
                Deelnemen: https://teams.microsoft.com/meet/123456789?p=abc\r\n\
                Vergadering-id: 123 456 789\r\n";
    assert_eq!(without_text_block(text, &teams), "Notes first.");
}
