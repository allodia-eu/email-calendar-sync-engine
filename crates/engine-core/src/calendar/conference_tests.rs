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

#[test]
fn a_link_joins_a_meeting_that_has_it_or_that_is_the_events_one_on_its_service() {
    let mut teams = group_flat(vec![entry(
        "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0",
    )])
    .remove(0);
    let short = ConferenceUri::parse("https://teams.microsoft.com/meet/123?p=x").unwrap();
    assert!(
        teams.joined_at(
            &ConferenceUri::parse("https://Teams.microsoft.com/l/meetup-join/19%3Ameeting_x/0/")
                .unwrap()
        )
    );
    assert!(
        !teams.joined_at(&short),
        "an attached meeting is not the service's only one"
    );
    teams.origin = ConferenceOrigin::Native {
        removable: false,
        state: NativeState::Ready,
    };
    assert!(
        teams.joined_at(&short),
        "the server's own meeting is the event's one on Teams"
    );
    assert!(!teams.joined_at(&ConferenceUri::parse("https://zoom.us/j/1").unwrap()));
}

#[test]
fn a_dial_in_number_is_the_label_when_it_is_written_as_one() {
    let mut point = EntryPoint::at(ConferenceUri::parse("tel:+14255550100,,123456789#").unwrap());
    assert_eq!(point.phone_number(), Some("+14255550100"));
    point.label = Some("+1 425-555-0100".into());
    assert_eq!(point.phone_number(), Some("+1 425-555-0100"));
    point.label = Some("(020) 555.0100".into());
    assert_eq!(point.phone_number(), Some("(020) 555.0100"));
    point.label = Some("Dial in".into());
    assert_eq!(point.phone_number(), Some("+14255550100"));
    let web = EntryPoint::at(ConferenceUri::parse("https://zoom.us/j/1").unwrap());
    assert_eq!(web.phone_number(), None);
}

#[test]
fn a_way_in_offers_what_it_states_else_what_its_kind_offers() {
    let web = EntryPoint::at(ConferenceUri::parse("https://meet.example.org/a").unwrap());
    assert_eq!(
        web.offered_features(),
        [ConferenceFeature::Audio, ConferenceFeature::Video].into()
    );
    let phone = EntryPoint::at(ConferenceUri::parse("tel:+31201234567").unwrap());
    assert_eq!(phone.offered_features(), [ConferenceFeature::Phone].into());
    let sip = EntryPoint::at(ConferenceUri::parse("sip:room@example.org").unwrap());
    assert!(sip.offered_features().is_empty());
    let mut stated = web;
    stated.features = [ConferenceFeature::Chat].into();
    assert_eq!(stated.offered_features(), [ConferenceFeature::Chat].into());
}

#[test]
fn the_join_link_comes_first_under_the_meetings_name() {
    let mut dial = EntryPoint::at(ConferenceUri::parse("tel:+31201234567").unwrap());
    dial.label = Some("+31 20 1234567".into());
    let mut web = EntryPoint::at(ConferenceUri::parse("https://meet.example.org/a").unwrap());
    web.label = Some("Open in a browser".into());
    let conference = Conference {
        key: ConferenceKey::new("k"),
        service: None,
        origin: ConferenceOrigin::Attached,
        entry_points: vec![dial, web],
        label: Some("Team call".into()),
        meeting_id: None,
        notes: None,
    };
    let ways: Vec<(&str, Option<&str>)> = conference
        .labelled_ways_in()
        .map(|(point, label)| (point.uri.as_str(), label))
        .collect();
    assert_eq!(
        ways,
        [
            ("https://meet.example.org/a", Some("Team call")),
            ("tel:+31201234567", Some("+31 20 1234567")),
        ]
    );
}
