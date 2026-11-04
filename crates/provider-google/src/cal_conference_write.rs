//! Putting a video meeting on a Google event, and taking one off: `conferenceData`, which the
//! API reads only under `conferenceDataVersion=1` (measured on a test account).
//!
//! - **Meet** is a `createRequest` for `hangoutsMeet`. Its `requestId` is derived from the event
//!   and the edit's stamp, so a retried write asks for the same meeting.
//! - **A meeting made elsewhere** is an `addOn` conference carrying its ways in. Google Calendar
//!   shows it with a Join button, and the invitation Google sends puts its link in its own block of
//!   the description and in `LOCATION`, so no block of ours is written.
//! - **Removing** one is `conferenceData: null`. A meeting the engine found in the text rather than
//!   in `conferenceData` comes out of the description and the location instead.

use engine_core::{
    calendar::{
        Conference, ConferenceKey, ConferenceService, EntryKind, EntryPoint, Event,
        without_text_block,
    },
    time::UtcDateTime,
};
use engine_provider::{
    EventDraft, EventPatch, NewConference, ProviderError, ProviderResult, TextEdit,
};
use serde_json::{Map, Value, json};

/// The query parameter a write carrying a meeting edit needs: without it Google ignores
/// `conferenceData`.
pub(crate) fn version_query(carries_meeting: bool) -> &'static str {
    if carries_meeting {
        "&conferenceDataVersion=1"
    } else {
        ""
    }
}

/// The key the read model gives the conference `conferenceData` holds.
const KEY: &str = "conferenceData";

/// The `conferenceData` a create carries, when it puts a meeting on the event.
pub(crate) fn for_create(draft: &EventDraft) -> ProviderResult<Option<Value>> {
    draft
        .conference
        .as_ref()
        .map(|new| conference_data(new, draft.uid.as_str(), draft.stamp))
        .transpose()
}

/// Adds what `patch`'s conference edit writes to `body`. A patch that carries one is sent
/// with [`version_query`].
pub(crate) fn apply_to_patch(
    base: &Event,
    patch: &EventPatch,
    body: &mut Map<String, Value>,
) -> ProviderResult<()> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(());
    };
    if let Some(key) = edit.removes() {
        let conference = held(base, key)?;
        if key.as_str() == KEY {
            body.insert(KEY.to_owned(), Value::Null);
        } else {
            remove_from_text(base, patch, conference, body);
        }
    }
    if let Some(new) = edit.adds() {
        body.insert(
            KEY.to_owned(),
            conference_data(new, base.uid.as_str(), patch.stamp())?,
        );
    }
    Ok(())
}

/// The conference `key` names on `base`.
fn held<'a>(base: &'a Event, key: &ConferenceKey) -> ProviderResult<&'a Conference> {
    base.conferences
        .iter()
        .find(|conference| &conference.key == key)
        .ok_or_else(|| ProviderError::invalid_state("the event holds no meeting with that key"))
}

/// Takes a meeting found in the text out of the description, and out of the location when
/// the location is exactly its link. An edit of either in the same patch is what is cut.
fn remove_from_text(
    base: &Event,
    patch: &EventPatch,
    conference: &Conference,
    body: &mut Map<String, Value>,
) {
    let description = match patch.description_edit() {
        Some(TextEdit::Set(text)) => Some(text.as_str()),
        Some(TextEdit::Clear) => None,
        None => base.description.as_deref(),
    };
    if let Some(description) = description {
        let kept = without_text_block(description, conference);
        body.insert("description".to_owned(), json!(kept));
    }
    let location = match patch.location_edit() {
        Some(TextEdit::Set(text)) => Some(text.as_str()),
        Some(TextEdit::Clear) => None,
        None => base
            .locations
            .first()
            .and_then(|location| location.name.as_deref()),
    };
    if location.is_some_and(|location| {
        conference
            .join()
            .is_some_and(|uri| uri.as_str() == location)
    }) {
        body.insert("location".to_owned(), json!(""));
    }
}

/// The `conferenceData` that puts `new` on an event.
fn conference_data(new: &NewConference, uid: &str, stamp: UtcDateTime) -> ProviderResult<Value> {
    match new {
        NewConference::Native(ConferenceService::GoogleMeet) => Ok(json!({
            "createRequest": {
                "requestId": request_id(uid, stamp),
                "conferenceSolutionKey": { "type": "hangoutsMeet" }
            }
        })),
        NewConference::Native(other) => Err(ProviderError::invalid_state(format!(
            "a Google calendar makes Meet meetings, not {other:?}"
        ))),
        NewConference::Attach(attached) => {
            let (conference, text) = (&attached.conference, &attached.text);
            if conference.join().is_none() {
                return Err(ProviderError::invalid_state(
                    "a meeting to attach needs a web link to join it at",
                ));
            }
            let entry_points: Vec<Value> =
                conference.entry_points.iter().map(entry_point).collect();
            Ok(json!({
                "conferenceSolution": {
                    "key": { "type": "addOn" },
                    "name": conference.label.as_deref().unwrap_or(&text.meeting)
                },
                "entryPoints": entry_points
            }))
        }
    }
}

/// A `createRequest.requestId`: the same for every retry of one write, different for the next
/// write that asks again.
fn request_id(uid: &str, stamp: UtcDateTime) -> String {
    format!("{uid}-{stamp}")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// One of Google's `entryPoints`, its codes under Google's own names.
fn entry_point(entry: &EntryPoint) -> Value {
    let kind = match entry.kind {
        EntryKind::Video => "video",
        EntryKind::Phone => "phone",
        EntryKind::Sip => "sip",
        _ => "more",
    };
    let mut point = Map::new();
    point.insert("entryPointType".to_owned(), json!(kind));
    point.insert("uri".to_owned(), json!(entry.uri.as_str()));
    let label = entry.label.as_deref().or_else(|| entry.phone_number());
    if let Some(label) = label {
        point.insert("label".to_owned(), json!(label));
    }
    let codes = [
        ("pin", &entry.access.pin),
        ("accessCode", &entry.access.access_code),
        ("meetingCode", &entry.access.meeting_code),
        ("passcode", &entry.access.passcode),
        ("password", &entry.access.password),
    ];
    for (name, code) in codes {
        if let Some(code) = code {
            point.insert(name.to_owned(), json!(code));
        }
    }
    Value::Object(point)
}
