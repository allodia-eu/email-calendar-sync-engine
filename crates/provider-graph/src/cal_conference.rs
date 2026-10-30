//! Reading a Graph event's online meeting into the event's conferences.
//!
//! Graph describes at most one, in `isOnlineMeeting`, `onlineMeetingProvider` and
//! `onlineMeeting` (`joinUrl`, `conferenceId` and the dial-in numbers). Exchange made it, so
//! it is the calendar's own, and it stays: "You cannot change the onlineMeetingProvider
//! property, nor set isOnlineMeeting to false" (Graph's `event` reference). An event marked
//! online with a known provider and no `onlineMeeting` yet reads as pending, since Exchange
//! fills it in after the write. A join URL that is there but cannot be opened safely is no
//! conference, never a pending one.

use engine_core::calendar::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, EntryKind,
    EntryPoint, NativeState,
};
use serde_json::Value;

use crate::json::{bool_field, opt_str};

/// The key of Graph's one conference: the field it lives in.
const KEY: &str = "onlineMeeting";

/// The event's online meeting, if it has one.
pub(crate) fn conferences(value: &Value) -> Vec<Conference> {
    let meeting = value.get("onlineMeeting").filter(|m| m.is_object());
    let written = meeting
        .and_then(|m| opt_str(m, "joinUrl"))
        .or_else(|| opt_str(value, "onlineMeetingUrl"))
        .filter(|url| !url.is_empty());
    let join = match written {
        Some(url) => match ConferenceUri::parse(url) {
            Ok(uri) => Some(uri),
            Err(_) => return Vec::new(),
        },
        None => None,
    };
    let provider = opt_str(value, "onlineMeetingProvider");
    let known_provider = matches!(
        provider,
        Some("teamsForBusiness" | "skypeForBusiness" | "skypeForConsumer")
    );
    if join.is_none() && !(bool_field(value, "isOnlineMeeting") && known_provider) {
        return Vec::new();
    }
    let mut entry_points: Vec<EntryPoint> = join.iter().cloned().map(EntryPoint::at).collect();
    for number in meeting.map(dial_in_numbers).unwrap_or_default() {
        if let Some(entry) = phone(number)
            && !entry_points.iter().any(|known| known.uri == entry.uri)
        {
            entry_points.push(entry);
        }
    }
    let service = match provider {
        Some("teamsForBusiness") => Some(ConferenceService::Teams),
        Some("skypeForBusiness") => Some(ConferenceService::SkypeForBusiness),
        Some("skypeForConsumer") => Some(ConferenceService::Skype),
        _ => join.as_ref().and_then(ConferenceService::recognise),
    };
    let state = if join.is_some() {
        NativeState::Ready
    } else {
        NativeState::Pending
    };
    vec![Conference {
        key: ConferenceKey::new(KEY),
        service,
        origin: ConferenceOrigin::Native {
            removable: false,
            state,
        },
        entry_points,
        label: None,
        meeting_id: meeting
            .and_then(|m| opt_str(m, "conferenceId"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        notes: None,
    }]
}

/// Every dial-in number `onlineMeeting` names: `tollNumber`, `tollFreeNumbers` and
/// `phones[].number`, as written.
fn dial_in_numbers(meeting: &Value) -> Vec<&str> {
    let toll = opt_str(meeting, "tollNumber");
    let toll_free = meeting
        .get("tollFreeNumbers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    let phones = meeting
        .get("phones")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|phone| opt_str(phone, "number"));
    toll.into_iter().chain(toll_free).chain(phones).collect()
}

/// A dial-in number as a `tel:` way in, labelled with the number as Graph wrote it. The URI
/// keeps the digits and a leading `+`, which is what RFC 3966 needs to dial; `None` when
/// there are no digits.
fn phone(number: &str) -> Option<EntryPoint> {
    let digits: String = number
        .trim()
        .char_indices()
        .filter(|&(i, c)| c.is_ascii_digit() || (i == 0 && c == '+'))
        .map(|(_, c)| c)
        .collect();
    if !digits.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut entry = EntryPoint::at(ConferenceUri::parse(&format!("tel:{digits}")).ok()?);
    entry.kind = EntryKind::Phone;
    entry.label = Some(number.trim().to_owned());
    Some(entry)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ONLINE: &str = include_str!("../tests/fixtures/calendar/event_online_meeting.json");

    fn only(value: &Value) -> Conference {
        let mut found = conferences(value);
        assert_eq!(found.len(), 1, "{found:?}");
        found.remove(0)
    }

    #[test]
    fn a_teams_meeting_is_the_calendars_own_and_stays() {
        let teams = only(&serde_json::from_str(ONLINE).unwrap());
        assert_eq!(teams.key.as_str(), "onlineMeeting");
        assert_eq!(teams.service, Some(ConferenceService::Teams));
        assert_eq!(
            teams.origin,
            ConferenceOrigin::Native {
                removable: false,
                state: NativeState::Ready
            }
        );
        assert_eq!(
            teams.join().map(ConferenceUri::as_str),
            Some("https://teams.live.com/meet/9990000000000?p=EXAMPLEpasscode000")
        );
        assert_eq!(teams.entry_points.len(), 1);
    }

    #[test]
    fn every_dial_in_number_is_a_way_in_and_the_conference_id_is_the_meeting_id() {
        // The `onlineMeetingInfo` shape of Graph's reference, with one number repeated.
        let event = json!({
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "teamsForBusiness",
            "onlineMeeting": {
                "joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0",
                "conferenceId": "177513992",
                "tollNumber": "+1 425-555-0100",
                "tollFreeNumbers": ["+1 800-555-0100"],
                "phones": [{ "number": "+1 425-555-0100", "type": "business" }, { "number": "n/a" }],
                "quickDial": "+1 425-555-0100,,177513992#"
            }
        });
        let teams = only(&event);
        assert_eq!(teams.meeting_id.as_deref(), Some("177513992"));
        let ways: Vec<(&str, Option<&str>)> = teams
            .entry_points
            .iter()
            .map(|e| (e.uri.as_str(), e.label.as_deref()))
            .collect();
        assert_eq!(
            ways,
            [
                (
                    "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0",
                    None
                ),
                ("tel:+14255550100", Some("+1 425-555-0100")),
                ("tel:+18005550100", Some("+1 800-555-0100")),
            ]
        );
        assert_eq!(teams.entry_points[1].kind, EntryKind::Phone);
    }

    #[test]
    fn an_online_meeting_exchange_has_not_provisioned_is_pending() {
        let event = json!({ "isOnlineMeeting": true, "onlineMeetingProvider": "teamsForBusiness", "onlineMeeting": null });
        let teams = only(&event);
        assert_eq!(
            teams.origin,
            ConferenceOrigin::Native {
                removable: false,
                state: NativeState::Pending
            }
        );
        assert_eq!(teams.entry_points, []);
    }

    #[test]
    fn an_older_provider_reads_from_its_own_url_and_an_unknown_one_from_the_link() {
        let skype = only(&json!({
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "skypeForConsumer",
            "onlineMeetingUrl": "https://join.skype.com/abcDEF"
        }));
        assert_eq!(skype.service, Some(ConferenceService::Skype));
        let unknown = only(&json!({
            "isOnlineMeeting": true,
            "onlineMeetingProvider": "unknown",
            "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/meet/123?p=x" }
        }));
        assert_eq!(unknown.service, Some(ConferenceService::Teams));
    }

    #[test]
    fn a_join_url_that_cannot_be_opened_or_no_provider_is_no_conference_not_a_pending_one() {
        let refused = json!({
            "isOnlineMeeting": true, "onlineMeetingProvider": "teamsForBusiness",
            "onlineMeeting": { "joinUrl": "https://user@teams.example.com/meet/1" }
        });
        assert_eq!(conferences(&refused), []);
        let unknown = json!({ "isOnlineMeeting": true, "onlineMeetingProvider": "unknown" });
        assert_eq!(conferences(&unknown), []);
    }

    #[test]
    fn an_event_that_is_not_online_has_no_conference() {
        assert_eq!(
            conferences(&json!({ "isOnlineMeeting": false, "onlineMeeting": null })),
            []
        );
        assert_eq!(conferences(&json!({})), []);
    }
}
