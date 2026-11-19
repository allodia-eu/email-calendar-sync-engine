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
use engine_provider::{EventDraft, EventPatch, NewConference, ProviderError, ProviderResult};
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
        let conference = base.conference(key).ok_or_else(|| {
            ProviderError::invalid_state("the event holds no meeting with that key")
        })?;
        if key.as_str() == KEY {
            body.insert(KEY.to_owned(), Value::Null);
        } else {
            remove_from_text(base, patch, conference, body);
        }
    }
    if let Some(new) = edit.adds() {
        // `conferenceData` holds one meeting, so a new one replaces it: the caller names it.
        let replaces_held = edit.removes().is_some_and(|key| key.as_str() == KEY);
        if !replaces_held && base.conference(&ConferenceKey::new(KEY)).is_some() {
            return Err(ProviderError::invalid_state(
                "a Google event holds one meeting; name the one it holds in `replacing`",
            ));
        }
        body.insert(
            KEY.to_owned(),
            conference_data(new, base.uid.as_str(), patch.stamp())?,
        );
    }
    Ok(())
}

/// Takes a meeting found in the text out of the description (an edit of it in the same patch
/// is what is cut), and out of the stored location when that is exactly its link. A location
/// the same patch sets is the person's and stays.
fn remove_from_text(
    base: &Event,
    patch: &EventPatch,
    conference: &Conference,
    body: &mut Map<String, Value>,
) {
    if let Some(description) = patch.description_after(base) {
        let kept = without_text_block(description, conference);
        if kept != description {
            body.insert("description".to_owned(), json!(kept));
        }
    }
    let location = match patch.location_edit() {
        Some(_) => None,
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
