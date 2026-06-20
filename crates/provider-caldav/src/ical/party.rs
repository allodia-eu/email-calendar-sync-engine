//! Mapping iCalendar `ORGANIZER`/`ATTENDEE`, `LOCATION`, and `CONFERENCE`
//! properties into the engine's participant and location model.
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
    Location, Participant, ParticipantRole, ParticipationStatus, VirtualLocation,
};

use super::component::Component;
use super::unfold::{ContentLine, unescape_text};

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

/// Maps every RFC 7986 `CONFERENCE` property to a [`VirtualLocation`].
pub(crate) fn parse_conferences(vevent: &Component) -> Vec<VirtualLocation> {
    vevent
        .all_properties("CONFERENCE")
        .map(|line| {
            let mut conference = VirtualLocation::new(line.value.trim());
            conference.name = line.param("LABEL").map(str::to_owned);
            if let Some(features) = line.param("FEATURE") {
                // JSCalendar feature keys are lowercase ("video"); iCalendar
                // `FEATURE` tokens are uppercase. Normalize to the projection form.
                conference.features = features
                    .split(',')
                    .map(|feature| feature.trim().to_ascii_lowercase())
                    .filter(|feature| !feature.is_empty())
                    .collect();
            }
            conference
        })
        .collect()
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
    use super::super::component::parse_components;
    use super::*;

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
    fn conference_maps_to_a_virtual_location() {
        let event = vevent(
            "CONFERENCE;VALUE=URI;FEATURE=VIDEO;LABEL=Join the meeting:https://meet.example.com/harness-room",
        );
        let conferences = parse_conferences(&event);
        assert_eq!(conferences.len(), 1);
        let conference = &conferences[0];
        assert_eq!(conference.uri, "https://meet.example.com/harness-room");
        assert_eq!(conference.name.as_deref(), Some("Join the meeting"));
        assert!(conference.features.contains("video"));
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
