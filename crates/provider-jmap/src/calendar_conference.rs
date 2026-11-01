//! Reading JSCalendar `virtualLocations` (RFC 8984 §4.2.6) into the event's conferences.

use engine_core::calendar::{Conference, ConferenceFeature, ConferenceUri, FlatEntry, group_flat};
use serde_json::Value;

use crate::json::{opt_str, true_keys};

/// The `virtualLocations` map as conferences ([`group_flat`]). An entry without a `uri`, or
/// whose `uri` is not one a person can join through, is left out.
pub(crate) fn parse_conferences(value: &Value) -> Vec<Conference> {
    let Some(map) = value.get("virtualLocations").and_then(Value::as_object) else {
        return Vec::new();
    };
    let entries = map
        .iter()
        .filter_map(|(key, location)| {
            let mut entry = FlatEntry::new(ConferenceUri::parse(opt_str(location, "uri")?).ok()?);
            entry.key = Some(key.clone());
            entry.label = opt_str(location, "name").map(str::to_owned);
            entry.description = opt_str(location, "description").map(str::to_owned);
            entry.features = true_keys(location, "features")
                .map(ConferenceFeature::from_wire)
                .collect();
            Some(entry)
        })
        .collect();
    group_flat(entries)
}

#[cfg(test)]
mod tests {
    use engine_core::calendar::{ConferenceOrigin, ConferenceService, EntryKind};
    use serde_json::json;

    use super::*;

    #[test]
    fn a_meeting_and_its_dial_in_read_as_one_conference_keyed_by_the_map() {
        let event = json!({ "virtualLocations": {
            "dial": { "@type": "VirtualLocation", "uri": "tel:+31201234567", "name": "Dial in" },
            "web": {
                "@type": "VirtualLocation", "uri": "https://meet.jit.si/ExampleRoom",
                "name": "Jitsi", "description": "Open in a browser",
                "features": { "video": true, "screen": true, "chat": false }
            }
        }});
        let conferences = parse_conferences(&event);
        assert_eq!(conferences.len(), 1);
        let meeting = &conferences[0];
        assert_eq!(meeting.key.as_str(), "web");
        assert_eq!(meeting.service, Some(ConferenceService::Jitsi));
        assert_eq!(meeting.origin, ConferenceOrigin::Attached);
        assert_eq!(meeting.label.as_deref(), Some("Jitsi"));
        assert_eq!(meeting.notes.as_deref(), Some("Open in a browser"));
        assert_eq!(
            meeting.entry_points[0].features,
            [ConferenceFeature::Video, ConferenceFeature::Screen]
                .into_iter()
                .collect()
        );
        assert_eq!(meeting.entry_points[1].kind, EntryKind::Phone);
        assert_eq!(meeting.entry_points[1].label.as_deref(), Some("Dial in"));
    }

    #[test]
    fn an_entry_without_a_joinable_uri_is_left_out() {
        let event = json!({ "virtualLocations": {
            "a": { "@type": "VirtualLocation" },
            "b": { "@type": "VirtualLocation", "uri": "javascript:alert(1)" }
        }});
        assert_eq!(parse_conferences(&event), []);
        assert_eq!(parse_conferences(&json!({})), []);
    }
}
