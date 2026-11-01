//! Reading a Google event's `conferenceData` into the event's conferences.
//!
//! Google describes at most one conference per event, with every way in it offers
//! (`entryPoints`), its ID and notes. One Google made itself (`hangoutsMeet`) is the
//! calendar's own and can be taken off again; one an add-on wrote (`addOn`) is stored, not
//! made. A request for a Meet conference that Google has not finished reads as pending, with
//! no way in yet.

use engine_core::calendar::{
    AccessCodes, Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri,
    EntryKind, EntryPoint, NativeState,
};
use serde_json::Value;

use crate::json::opt_str;

/// The key of Google's one conference: the field it lives in.
const KEY: &str = "conferenceData";

/// The event's conference: from `conferenceData`, else from the `hangoutLink` when
/// `conferenceData` is absent or describes nothing.
pub(crate) fn conferences(value: &Value) -> Vec<Conference> {
    value
        .get(KEY)
        .and_then(from_conference_data)
        .or_else(|| {
            let uri = ConferenceUri::parse(opt_str(value, "hangoutLink")?).ok()?;
            Some(Conference {
                key: ConferenceKey::new(KEY),
                service: Some(ConferenceService::GoogleMeet),
                origin: native(NativeState::Ready),
                entry_points: vec![EntryPoint::at(uri)],
                label: None,
                meeting_id: None,
                notes: None,
            })
        })
        .into_iter()
        .collect()
}

fn from_conference_data(data: &Value) -> Option<Conference> {
    let solution = data.get("conferenceSolution");
    let solution_type = solution
        .and_then(|s| s.get("key"))
        .and_then(|key| opt_str(key, "type"))
        .or_else(|| {
            data.get("createRequest")
                .and_then(|r| r.get("conferenceSolutionKey"))
                .and_then(|key| opt_str(key, "type"))
        });
    let mut entry_points: Vec<EntryPoint> = data
        .get("entryPoints")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(entry_point)
        .collect();
    // The way in a person uses first leads; Google lists the video one first in practice,
    // and nothing in its reference promises it.
    entry_points.sort_by_key(|entry| entry.kind != EntryKind::Video);
    let state = match data
        .get("createRequest")
        .and_then(|r| r.get("status"))
        .and_then(|s| opt_str(s, "statusCode"))
    {
        Some("pending") => NativeState::Pending,
        Some("failure") => NativeState::Failed,
        _ => NativeState::Ready,
    };
    // A conference with no way in is worth reporting only as Google's own request in
    // flight or failed; an add-on's request is the add-on's business, not the calendar's.
    let is_add_on = solution_type == Some("addOn");
    if entry_points.is_empty() && (state == NativeState::Ready || is_add_on) {
        return None;
    }
    let service = match solution_type {
        Some("hangoutsMeet") => Some(ConferenceService::GoogleMeet),
        _ => entry_points
            .iter()
            .find_map(|entry| ConferenceService::recognise(&entry.uri)),
    };
    let origin = if is_add_on {
        ConferenceOrigin::Attached
    } else {
        native(state)
    };
    Some(Conference {
        key: ConferenceKey::new(KEY),
        service,
        origin,
        entry_points,
        label: solution
            .and_then(|s| opt_str(s, "name"))
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        meeting_id: nonempty(data, "conferenceId"),
        notes: opt_str(data, "notes")
            .map(|html| engine_mime::html_to_text(html).trim().to_owned())
            .filter(|notes| !notes.is_empty()),
    })
}

/// A conference Google made and keeps; the event can be patched to drop it.
const fn native(state: NativeState) -> ConferenceOrigin {
    ConferenceOrigin::Native {
        removable: true,
        state,
    }
}

/// One `entryPoints[]` entry, or `None` when its `uri` is not one a person can join
/// through.
fn entry_point(entry: &Value) -> Option<EntryPoint> {
    let mut point = EntryPoint::at(ConferenceUri::parse(opt_str(entry, "uri")?).ok()?);
    if let Some(kind) = opt_str(entry, "entryPointType") {
        point.kind = EntryKind::from_wire(kind);
    }
    point.label = nonempty(entry, "label");
    point.access = AccessCodes {
        pin: nonempty(entry, "pin"),
        access_code: nonempty(entry, "accessCode"),
        meeting_code: nonempty(entry, "meetingCode"),
        passcode: nonempty(entry, "passcode"),
        password: nonempty(entry, "password"),
    };
    Some(point)
}

fn nonempty(value: &Value, key: &str) -> Option<String> {
    opt_str(value, key)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
#[path = "cal_conference_tests.rs"]
mod tests;
