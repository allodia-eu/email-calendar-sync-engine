//! Mapping iCalendar `ORGANIZER`/`ATTENDEE`, `LOCATION`, and `CONFERENCE`
//! properties into the engine's participant, location and conference model.
//!
//! iCalendar keeps `ORGANIZER` and `ATTENDEE` as separate properties, while
//! JSCalendar (the engine projection) merges them into one participant per
//! address with a *set* of roles. So an `ORGANIZER` that is also an `ATTENDEE`
//! (the common self-organized meeting) becomes a single participant carrying both
//! the `owner` role and the attendee's role/status — matching what the JMAP
//! adapter produces from JSCalendar. The role/status enum spellings differ between
//! the two formats and are mapped explicitly (`REQ-PARTICIPANT` → `attendee`,
//! `OPT-PARTICIPANT` → `optional`, …).

use std::collections::BTreeSet;

use engine_core::calendar::{
    Conference, ConferenceFeature, ConferenceUri, FlatEntry, Location, Participant,
    ParticipantRole, ParticipationStatus, group_flat,
};

use super::{
    component::Component,
    unfold::{ContentLine, unescape_text},
};

/// Collects the merged participants of a `VEVENT` (organizer + attendees).
pub(crate) fn parse_participants(vevent: &Component) -> Vec<Participant> {
    let mut participants: Vec<Participant> = Vec::new();
    if let Some(organizer) = vevent.property("ORGANIZER") {
        add_party(
            &mut participants,
            organizer,
            Some(ParticipantRole::Owner),
            false,
        );
    }
    for attendee in vevent.all_properties("ATTENDEE") {
        let role = attendee.param("ROLE").map(map_role);
        add_party(&mut participants, attendee, role, true);
    }
    participants
}

/// Maps every `LOCATION` property to a named physical [`Location`].
pub(crate) fn parse_locations(vevent: &Component) -> Vec<Location> {
    vevent
        .all_properties("LOCATION")
        .map(|line| Location::named(unescape_text(&line.value)))
        .collect()
}

/// Reads every RFC 7986 `CONFERENCE` property into the event's conferences
/// ([`group_flat`]). A value that is not a URI of a scheme a person can join through is
/// left out, along with an explicit `VALUE` other than `URI`, the only type §5.11 allows.
pub(crate) fn parse_conferences(vevent: &Component) -> Vec<Conference> {
    let entries = vevent
        .all_properties("CONFERENCE")
        .filter(|line| {
            line.param("VALUE")
                .is_none_or(|value| value.eq_ignore_ascii_case("URI"))
        })
        .filter_map(|line| {
            let mut entry = FlatEntry::new(ConferenceUri::parse(&line.value).ok()?);
            entry.label = line.param("LABEL").map(str::to_owned);
            if let Some(features) = line.param("FEATURE") {
                // iCalendar `FEATURE` tokens are uppercase; JSCalendar's keys are lowercase.
                entry.features = features
                    .split(',')
                    .map(|feature| feature.trim().to_ascii_lowercase())
                    .filter(|feature| !feature.is_empty())
                    .map(ConferenceFeature::from)
                    .collect();
            }
            Some(entry)
        })
        .collect();
    group_flat(entries)
}

/// Finds the participant for `line`'s address (creating one), then records its
/// role and — for an attendee — its participation status and RSVP flag.
fn add_party(
    out: &mut Vec<Participant>,
    line: &ContentLine,
    role: Option<ParticipantRole>,
    is_attendee: bool,
) {
    let email = cal_address(&line.value);
    let name = line.param("CN").map(str::to_owned);
    // An empty ORGANIZER/ATTENDEE (no cal-address and no CN) is noise, not a
    // participant — skip it rather than creating a phantom carrying only a role.
    if email.is_none() && name.is_none() {
        return;
    }
    let index = upsert_index(out, email.as_deref());
    let participant = &mut out[index];
    if participant.email.is_none() {
        participant.email = email;
    }
    if participant.name.is_none() {
        participant.name = name;
    }
    if let Some(role) = role {
        participant.roles.insert(role);
    }
    if is_attendee {
        if let Some(status) = line.param("PARTSTAT") {
            participant.participation_status =
                ParticipationStatus::from_wire(&status.to_ascii_lowercase());
        }
        if line
            .param("RSVP")
            .is_some_and(|v| v.eq_ignore_ascii_case("TRUE"))
        {
            participant.expect_reply = true;
        }
    }
}

/// Returns the index of the participant matching `email` (case-insensitive),
/// pushing a fresh one when there is no match or no address.
fn upsert_index(out: &mut Vec<Participant>, email: Option<&str>) -> usize {
    if let Some(email) = email
        && let Some(index) = out.iter().position(|p| {
            p.email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(email))
        })
    {
        return index;
    }
    out.push(Participant {
        name: None,
        email: None,
        kind: None,
        roles: BTreeSet::new(),
        participation_status: ParticipationStatus::NeedsAction,
        expect_reply: false,
        comment: None,
        sent_by: None,
    });
    out.len() - 1
}

/// Maps an iCalendar `ROLE` parameter to a [`ParticipantRole`].
fn map_role(role: &str) -> ParticipantRole {
    match role.to_ascii_uppercase().as_str() {
        "CHAIR" => ParticipantRole::Chair,
        "REQ-PARTICIPANT" => ParticipantRole::Attendee,
        "OPT-PARTICIPANT" => ParticipantRole::Optional,
        "NON-PARTICIPANT" => ParticipantRole::Informational,
        other => ParticipantRole::from_wire(&other.to_ascii_lowercase()),
    }
}

/// Extracts the bare address from a `mailto:`-scheme cal-address, or `None` when
/// empty.
fn cal_address(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let address = trimmed
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
        .map_or(trimmed, |_| &trimmed[7..]);
    (!address.is_empty()).then(|| address.to_owned())
}

#[cfg(test)]
mod tests {
    use engine_core::calendar::{ConferenceOrigin, ConferenceService, EntryKind};

    use super::{super::component::parse_components, *};

    fn vevent(body: &str) -> Component {
        let text =
            format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\n{body}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        parse_components(&text)
            .into_iter()
            .next()
            .unwrap()
            .children
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn organizer_and_attendees_merge_by_address() {
        // The seed meeting-attendees fixture, where alice is both organizer and
        // chair: she becomes one participant carrying both roles.
        let event = vevent(
            "ORGANIZER;CN=Alice Tester:mailto:alice@test.local\r\n\
             ATTENDEE;CN=Alice Tester;ROLE=CHAIR;PARTSTAT=ACCEPTED;RSVP=FALSE:mailto:alice@test.local\r\n\
             ATTENDEE;CN=Bob Tester;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@test.local\r\n\
             ATTENDEE;CN=Carol External;ROLE=OPT-PARTICIPANT;PARTSTAT=TENTATIVE;RSVP=TRUE:mailto:carol@example.com",
        );
        let participants = parse_participants(&event);
        assert_eq!(participants.len(), 3, "alice merges; not four rows");

        let alice = &participants[0];
        assert_eq!(alice.email.as_deref(), Some("alice@test.local"));
        assert_eq!(alice.name.as_deref(), Some("Alice Tester"));
        assert!(alice.roles.contains(&ParticipantRole::Owner));
        assert!(alice.roles.contains(&ParticipantRole::Chair));
        assert_eq!(alice.participation_status, ParticipationStatus::Accepted);
        assert!(!alice.expect_reply);

        let bob = &participants[1];
        assert!(bob.roles.contains(&ParticipantRole::Attendee));
        assert_eq!(bob.participation_status, ParticipationStatus::NeedsAction);
        assert!(bob.expect_reply);

        let carol = &participants[2];
        assert!(carol.roles.contains(&ParticipantRole::Optional));
        assert_eq!(carol.participation_status, ParticipationStatus::Tentative);
    }

    #[test]
    fn conference_maps_to_an_attached_conference() {
        let event = vevent(
            "CONFERENCE;VALUE=URI;FEATURE=VIDEO;LABEL=Join the meeting:https://meet.example.com/harness-room",
        );
        let conferences = parse_conferences(&event);
        assert_eq!(conferences.len(), 1);
        let conference = &conferences[0];
        assert_eq!(conference.origin, ConferenceOrigin::Attached);
        assert_eq!(conference.label.as_deref(), Some("Join the meeting"));
        let join = &conference.entry_points[0];
        assert_eq!(join.uri.as_str(), "https://meet.example.com/harness-room");
        assert_eq!(join.kind, EntryKind::Video);
        assert!(join.features.contains(&ConferenceFeature::Video));
    }

    #[test]
    fn nextcloud_talk_with_a_dial_in_reads_as_one_meeting() {
        // The shape Nextcloud's calendar writes for a Talk room, plus a dial-in line.
        let event = vevent(
            "CONFERENCE;VALUE=URI;FEATURE=AUDIO,VIDEO,CHAT;LABEL=Nextcloud Talk:https://cloud.example.com/call/abcd1234\r\n\
             CONFERENCE;VALUE=URI;FEATURE=PHONE;LABEL=Nextcloud Talk:tel:+31201234567",
        );
        let conferences = parse_conferences(&event);
        assert_eq!(conferences.len(), 1);
        let talk = &conferences[0];
        assert_eq!(talk.service, Some(ConferenceService::NextcloudTalk));
        assert_eq!(talk.entry_points.len(), 2);
        assert_eq!(talk.entry_points[1].kind, EntryKind::Phone);
        assert_eq!(
            talk.entry_points[0].features,
            [
                ConferenceFeature::Audio,
                ConferenceFeature::Video,
                ConferenceFeature::Chat
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn a_conference_no_host_may_open_is_left_out() {
        let event = vevent(
            "CONFERENCE;VALUE=URI:javascript:alert(1)\r\n\
             CONFERENCE;VALUE=TEXT:https://meet.example.com/text-typed\r\n\
             CONFERENCE:not a uri",
        );
        assert_eq!(parse_conferences(&event), []);
    }

    #[test]
    fn location_is_unescaped() {
        let event = vevent(r"LOCATION:Amsterdam HQ\, 3rd floor");
        let locations = parse_locations(&event);
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].name.as_deref(),
            Some("Amsterdam HQ, 3rd floor")
        );
    }

    #[test]
    fn empty_organizer_and_attendee_are_not_phantom_participants() {
        // An empty ORGANIZER / bare `mailto:` ATTENDEE with no CN must not create a
        // participant carrying only a role; a CN-only attendee is still kept.
        let event = vevent("ORGANIZER:\r\nATTENDEE:mailto:\r\nATTENDEE;CN=Named Only:invalid");
        let participants = parse_participants(&event);
        // Only the CN-bearing attendee survives (its value isn't a mailto address,
        // so it has no email, but it has a name).
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0].name.as_deref(), Some("Named Only"));
    }
}
