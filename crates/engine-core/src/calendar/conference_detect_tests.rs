use super::*;
use crate::calendar::{NativeState, group_flat};

fn uri(value: &str) -> ConferenceUri {
    ConferenceUri::parse(value).unwrap()
}

fn detect(held: Vec<Conference>, sources: &DetectionSources<'_>) -> Vec<Conference> {
    let mut conferences = held;
    add_detected(&mut conferences, sources);
    conferences
}

fn found(conferences: &[Conference]) -> Vec<(&str, ConferenceOrigin)> {
    conferences
        .iter()
        .map(|c| (c.entry_points[0].uri.as_str(), c.origin))
        .collect()
}

const fn at(place: DetectedIn) -> ConferenceOrigin {
    ConferenceOrigin::Detected { place }
}

#[test]
fn a_meeting_link_in_plain_text_or_html_is_a_conference_and_any_other_link_is_not() {
    let description = "Agenda<https://intranet.example.com/agenda>\r\n\
        Click here to join the meeting<https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d>\r\n\
        Learn more<https://www.microsoft.com/microsoft-teams/join-a-meeting>\r\n\
        <a href=\"https://us02web.zoom.us/j/123?pwd=abc&amp;from=addon\">Zoom</a>. \
        Or https://meet.google.com/abc-defg-hij.";
    let conferences = detect(
        Vec::new(),
        &DetectionSources {
            description: Some(description),
            ..DetectionSources::default()
        },
    );
    assert_eq!(
        found(&conferences),
        [
            (
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d",
                at(DetectedIn::Description)
            ),
            (
                "https://us02web.zoom.us/j/123?pwd=abc&from=addon",
                at(DetectedIn::Description)
            ),
            (
                "https://meet.google.com/abc-defg-hij",
                at(DetectedIn::Description)
            ),
        ]
    );
    let services: Vec<_> = conferences.iter().map(|c| c.service.clone()).collect();
    assert_eq!(
        services,
        [
            Some(ConferenceService::Teams),
            Some(ConferenceService::Zoom),
            Some(ConferenceService::GoogleMeet)
        ]
    );
    let zoom = &conferences[1];
    assert_eq!(
        zoom.key.as_str(),
        "https://us02web.zoom.us/j/123?pwd=abc&from=addon"
    );
    assert_eq!(zoom.join(), Some(&zoom.entry_points[0].uri));
}

#[test]
fn a_location_that_is_only_a_link_is_a_conference_whatever_it_links_to() {
    let locations = [
        Location::named(" https://video.example.com/room-12 "),
        Location::named("Room 4, see https://intranet.example.com/map"),
        Location::named("Zoom: https://zoom.us/j/987"),
    ];
    let conferences = detect(
        Vec::new(),
        &DetectionSources {
            locations: &locations,
            ..DetectionSources::default()
        },
    );
    assert_eq!(
        found(&conferences),
        [
            (
                "https://video.example.com/room-12",
                at(DetectedIn::Location)
            ),
            ("https://zoom.us/j/987", at(DetectedIn::Location)),
        ]
    );
    assert_eq!(conferences[0].service, None);
}

#[test]
fn one_meeting_named_in_several_places_keeps_its_strongest_origin() {
    let native = Conference {
        origin: ConferenceOrigin::Native {
            removable: false,
            state: NativeState::Ready,
        },
        ..group_flat(vec![super::super::FlatEntry::new(uri(
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d",
        ))])
        .remove(0)
    };
    let vendor = [
        uri("https://teams.microsoft.com/l/meetup-join/19%3Ameeting_x/0?context=%7B%7D"),
        uri("https://meet.google.com/abc-defg-hij"),
    ];
    let locations = [Location::named("https://meet.google.com/abc-defg-hij")];
    let conferences = detect(
        vec![native.clone()],
        &DetectionSources {
            vendor: &vendor,
            locations: &locations,
            description: Some(
                "Join: https://meet.google.com/abc-defg-hij \
                 https://zoom.us/j/1?pwd=x https://zoom.us/j/1",
            ),
        },
    );
    assert_eq!(conferences[0], native);
    assert_eq!(
        found(&conferences[1..]),
        [
            (
                "https://meet.google.com/abc-defg-hij",
                at(DetectedIn::VendorProperty)
            ),
            ("https://zoom.us/j/1?pwd=x", at(DetectedIn::Description)),
        ]
    );
}

#[test]
fn a_link_on_the_service_of_the_servers_own_meeting_is_that_meeting() {
    // Live Exchange: `joinUrl` is the long `meetup-join` link, the body the short `meet/` one.
    let mut teams = group_flat(vec![super::super::FlatEntry::new(uri(
        "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d",
    ))])
    .remove(0);
    teams.origin = ConferenceOrigin::Native {
        removable: false,
        state: NativeState::Ready,
    };
    let body = "Join: https://teams.microsoft.com/meet/123?p=x or https://zoom.us/j/1";
    let conferences = detect(
        vec![teams.clone()],
        &DetectionSources {
            description: Some(body),
            ..DetectionSources::default()
        },
    );
    assert_eq!(
        found(&conferences[1..]),
        [("https://zoom.us/j/1", at(DetectedIn::Description))]
    );
    // A meeting the server did not make is no such promise: a second Teams link is a second
    // meeting.
    teams.origin = ConferenceOrigin::Attached;
    let conferences = detect(
        vec![teams],
        &DetectionSources {
            description: Some(body),
            ..DetectionSources::default()
        },
    );
    assert_eq!(conferences.len(), 3);
}

#[test]
fn a_detected_conference_round_trips_through_its_stored_form() {
    let conference = detect(
        Vec::new(),
        &DetectionSources {
            description: Some("https://zoom.us/j/1"),
            ..DetectionSources::default()
        },
    )
    .remove(0);
    let json = serde_json::to_value(&conference).unwrap();
    assert_eq!(
        json["origin"],
        serde_json::json!({ "type": "detected", "place": "description" })
    );
    assert_eq!(
        serde_json::from_value::<Conference>(json).unwrap(),
        conference
    );
}

#[test]
fn a_link_on_the_service_a_vendor_property_names_is_that_meeting() {
    // An Exchange invitation: the property holds the long `meetup-join` link, the
    // description the short `meet/` one.
    let vendor = [uri(
        "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x/0?context=%7b%7d",
    )];
    let conferences = detect(
        Vec::new(),
        &DetectionSources {
            vendor: &vendor,
            description: Some("Join: https://teams.microsoft.com/meet/123?p=x"),
            ..DetectionSources::default()
        },
    );
    assert_eq!(conferences.len(), 1, "{conferences:?}");
    assert_eq!(conferences[0].origin, at(DetectedIn::VendorProperty));
}

#[test]
fn an_html_space_after_a_link_is_not_part_of_it() {
    let conferences = detect(
        Vec::new(),
        &DetectionSources {
            description: Some("<p>Join: https://zoom.us/j/1?pwd=x&nbsp;</p>"),
            ..DetectionSources::default()
        },
    );
    assert_eq!(
        found(&conferences),
        [("https://zoom.us/j/1?pwd=x", at(DetectedIn::Description))]
    );
}

#[test]
fn a_found_meeting_takes_the_fullest_link_and_a_stored_one_keeps_its_own() {
    let locations = [Location::named("Zoom: https://zoom.us/j/987")];
    let sources = DetectionSources {
        locations: &locations,
        description: Some("Join https://zoom.us/j/987?pwd=x"),
        ..DetectionSources::default()
    };
    let conferences = detect(Vec::new(), &sources);
    assert_eq!(
        found(&conferences),
        [("https://zoom.us/j/987?pwd=x", at(DetectedIn::Location))]
    );
    assert_eq!(conferences[0].key.as_str(), "https://zoom.us/j/987?pwd=x");

    let stored = group_flat(vec![super::super::FlatEntry::new(uri(
        "https://zoom.us/j/987",
    ))]);
    let conferences = detect(stored.clone(), &sources);
    assert_eq!(conferences, stored);
}

#[test]
fn a_link_google_wraps_in_its_own_is_read_once_through_the_plain_one() {
    // The shape of an invitation a Workspace calendar sends with a Zoom or Teams add-in: the
    // meeting in the location and as plain text, and again inside Google's redirect links, with
    // the meeting's chat beside it.
    let description = "Deelnemen Zoom Meeting\n\
        https://zoom.us/j/123456789?pwd=Abc.1&jst=2 (Id: 123456789)\n\
        <a href=\"https://www.google.com/url?q=https://zoom.us/j/123456789?pwd%3DAbc.1%26jst%3D2&amp;sa=D&amp;source=calendar\">\
        https://zoom.us/j/123456789?pwd=Abc.1&amp;jst=2</a>\n\
        <a href=\"https://www.google.com/url?q=https://teams.microsoft.com/l/chat/19:meeting_x%40thread.v2/conversations&amp;sa=D\">Chat</a>";
    let locations = [Location::named(
        "https://zoom.us/j/123456789?pwd=Abc.1&jst=2",
    )];
    let conferences = detect(
        Vec::new(),
        &DetectionSources {
            locations: &locations,
            description: Some(description),
            ..DetectionSources::default()
        },
    );
    assert_eq!(
        found(&conferences),
        [(
            "https://zoom.us/j/123456789?pwd=Abc.1&jst=2",
            at(DetectedIn::Location)
        )]
    );
}
