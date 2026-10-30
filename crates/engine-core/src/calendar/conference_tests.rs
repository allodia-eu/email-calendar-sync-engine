use super::*;

fn entry(uri: &str) -> FlatEntry {
    FlatEntry::new(ConferenceUri::parse(uri).unwrap())
}

fn uris(conference: &Conference) -> Vec<&str> {
    conference
        .entry_points
        .iter()
        .map(|e| e.uri.as_str())
        .collect()
}

#[test]
fn one_web_address_gathers_every_number_into_its_meeting() {
    let mut web = entry("https://meet.google.com/abc-defg-hij");
    web.label = Some("Google Meet".into());
    web.description = Some("Dial in for audio".into());
    web.features = [ConferenceFeature::Video].into_iter().collect();
    let mut dial = entry("tel:+31201234567");
    dial.key = Some("dial".into());
    dial.description = Some("PIN 1234#".into());
    let conferences = group_flat(vec![dial, web, entry("sip:123@example.com")]);
    assert_eq!(conferences.len(), 1);
    let meeting = &conferences[0];
    assert_eq!(
        uris(meeting),
        [
            "https://meet.google.com/abc-defg-hij",
            "tel:+31201234567",
            "sip:123@example.com"
        ]
    );
    assert_eq!(meeting.service, Some(ConferenceService::GoogleMeet));
    assert_eq!(meeting.origin, ConferenceOrigin::Attached);
    assert_eq!(meeting.label.as_deref(), Some("Google Meet"));
    assert_eq!(meeting.notes.as_deref(), Some("Dial in for audio"));
    assert_eq!(meeting.key.as_str(), "https://meet.google.com/abc-defg-hij");
    assert_eq!(
        meeting.entry_points[0].description, None,
        "the web one's is the notes"
    );
    assert_eq!(
        meeting.entry_points[1].description.as_deref(),
        Some("PIN 1234#")
    );
    let kinds: Vec<&EntryKind> = meeting.entry_points.iter().map(|e| &e.kind).collect();
    assert_eq!(
        kinds,
        [&EntryKind::Video, &EntryKind::Phone, &EntryKind::Sip]
    );
    assert_eq!(
        meeting.join().map(ConferenceUri::as_str),
        Some("https://meet.google.com/abc-defg-hij")
    );
}

#[test]
fn with_two_web_addresses_a_number_stands_alone() {
    let conferences = group_flat(vec![
        entry("https://meet.example.com/a"),
        entry("tel:+31201234567"),
        entry("https://meet.example.com/b"),
    ]);
    let grouped: Vec<Vec<&str>> = conferences.iter().map(uris).collect();
    assert_eq!(
        grouped,
        [
            vec!["https://meet.example.com/a"],
            vec!["https://meet.example.com/b"],
            vec!["tel:+31201234567"],
        ]
    );
    assert_eq!(conferences[2].service, None);
    assert_eq!(conferences[2].join(), None);
}

#[test]
fn a_format_key_names_the_meeting_and_a_repeated_address_is_read_once() {
    let mut keyed = entry("https://meet.example.com/a");
    keyed.key = Some("v1".into());
    let conferences = group_flat(vec![keyed, entry("https://meet.example.com/a")]);
    assert_eq!(conferences.len(), 1);
    assert_eq!(conferences[0].key.as_str(), "v1");
    assert_eq!(conferences[0].service, None);
}

#[test]
fn an_empty_list_is_no_meeting() {
    assert_eq!(group_flat(Vec::new()), Vec::new());
}

#[test]
fn a_conference_round_trips_through_its_stored_form() {
    let mut meeting = group_flat(vec![
        entry("https://meet.example.com/a"),
        entry("tel:+3120"),
    ])
    .remove(0);
    meeting.origin = ConferenceOrigin::Native {
        removable: false,
        state: NativeState::Pending,
    };
    meeting.entry_points[1].access.pin = Some("1234".into());
    let json = serde_json::to_value(&meeting).unwrap();
    assert_eq!(
        json["origin"],
        serde_json::json!({ "type": "native", "removable": false, "state": "pending" })
    );
    assert_eq!(serde_json::from_value::<Conference>(json).unwrap(), meeting);
}
