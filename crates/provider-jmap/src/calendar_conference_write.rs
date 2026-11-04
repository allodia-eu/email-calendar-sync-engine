//! Putting a video meeting on a JSCalendar event, and taking one off.
//!
//! - **A meeting made elsewhere** is one `virtualLocations` entry per way in (RFC 8984 §4.2.6), our
//!   block in the description, and its join link as the location when the event has none. Outlook,
//!   Google Calendar and Thunderbird read no `virtualLocations` in the invitation the server sends,
//!   so the block and the location are what an invitee on one of them sees.
//! - **Taking one off** removes every entry whose address joins it, its block, and the location
//!   when that is still its link.
//! - **A JMAP server makes no meetings of its own**; the adapter refuses one before anything is
//!   sent (`engine_provider::refuse_native_conference`).

use engine_core::calendar::{Conference, ConferenceUri, Event, with_text_block};
use engine_provider::{EventDraft, EventPatch, NewConference, TextEdit, meeting_text};
use serde_json::{Map, Value, json};

use crate::{
    calendar_patch::location_edit,
    calendar_write::{NEW_LOCATION_ID, escape_pointer},
    error::JmapError,
    json::opt_str,
};

/// Adds the meeting `draft` puts on the event to the created `object`, whose description and
/// locations the draft's own fields already set.
pub(crate) fn apply_to_create(
    draft: &EventDraft,
    object: &mut Map<String, Value>,
) -> Result<(), JmapError> {
    let Some(new) = &draft.conference else {
        return Ok(());
    };
    let (conference, block) = attached(new)?;
    let notes = with_text_block(draft.description.as_deref().unwrap_or_default(), &block);
    object.insert("description".to_owned(), json!(notes));
    if draft.location.is_none() {
        object.insert(
            "locations".to_owned(),
            json!({ NEW_LOCATION_ID: { "@type": "Location", "name": join(conference) } }),
        );
    }
    object.insert(
        "virtualLocations".to_owned(),
        Value::Object(entries(conference, &Map::new())),
    );
    Ok(())
}

/// What a patch's conference edit writes: the description and location pointers, which
/// replace the patch's own edits of them (they already include them), and the
/// `virtualLocations` pointers.
pub(crate) struct ConferenceWrite {
    pub(crate) description: Option<TextEdit>,
    pub(crate) location: Option<Vec<(String, Value)>>,
    pub(crate) virtual_locations: Vec<(String, Value)>,
}

/// Plans `patch`'s conference edit against `base`; `None` when the patch has none. The notes and
/// location come from [`meeting_text`]; what is this transport's own is which
/// `virtualLocations` entries and locations the edit names.
///
/// # Errors
///
/// [`JmapError::Protocol`] when the edit removes a meeting the event does not hold, asks the
/// server for a meeting of its own, or attaches one without a web link.
pub(crate) fn plan(base: &Event, patch: &EventPatch) -> Result<Option<ConferenceWrite>, JmapError> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(None);
    };
    let raw = raw(base);
    let stored = raw
        .as_ref()
        .and_then(|raw| raw.get("virtualLocations"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let stored_location = base
        .locations
        .first()
        .and_then(|location| location.name.as_deref())
        .unwrap_or_default();
    let text = meeting_text(base, patch, stored_location)
        .map_err(|error| JmapError::protocol(error.to_string()))?
        .unwrap_or_else(|| unreachable!("a patch with a conference edit has a text plan"));

    let mut removed = Map::new();
    if let Some(key) = edit.removes() {
        let conference = base
            .conference(key)
            .unwrap_or_else(|| unreachable!("meeting_text refused a key the event lacks"));
        for id in ids_where(&stored, |uri| conference.joined_at(uri)) {
            removed.insert(id, Value::Null);
        }
    }
    let mut added = Map::new();
    if let Some(new) = edit.adds() {
        let (conference, _) = attached(new)?;
        // Entries the event already holds for the same ways in are replaced, never repeated.
        for id in ids_where(&stored, |uri| {
            conference
                .entry_points
                .iter()
                .any(|point| point.uri == *uri)
        }) {
            removed.insert(id, Value::Null);
        }
        added = entries(conference, &stored);
    }
    // A cleared location is the removed meeting's link; only the locations named so go.
    let location = text.location.map(|location| match location {
        TextEdit::Clear => locations_named(raw.as_ref(), stored_location),
        set @ TextEdit::Set(_) => location_edit(base, &set),
    });

    // A pointer into a map the event does not have is an `invalidPatch` (RFC 8620 §5.3), so an
    // event without one is given the whole map.
    let virtual_locations = if stored.is_empty() {
        if added.is_empty() {
            Vec::new()
        } else {
            vec![("virtualLocations".to_owned(), Value::Object(added))]
        }
    } else {
        removed
            .into_iter()
            .chain(added)
            .map(|(id, value)| (format!("virtualLocations/{}", escape_pointer(&id)), value))
            .collect()
    };
    Ok(Some(ConferenceWrite {
        description: text.notes,
        location,
        virtual_locations,
    }))
}

/// The meeting a [`NewConference::Attach`] puts on, and its block.
fn attached(new: &NewConference) -> Result<(&Conference, String), JmapError> {
    let NewConference::Attach(attached) = new else {
        return Err(JmapError::protocol(
            "this calendar's server makes no meetings of its own",
        ));
    };
    let conference = &attached.conference;
    let block = conference
        .text_block(&attached.text)
        .ok_or_else(|| JmapError::protocol("a meeting to attach needs a web link to join it at"))?;
    Ok((conference, block))
}

fn join(conference: &Conference) -> String {
    conference
        .join()
        .map(|uri| uri.as_str().to_owned())
        .unwrap_or_default()
}

/// One `VirtualLocation` per way in ([`Conference::labelled_ways_in`]), at ids `stored` does
/// not use. The join link carries the meeting's notes, which is where the reader takes them
/// from.
fn entries(conference: &Conference, stored: &Map<String, Value>) -> Map<String, Value> {
    let join = conference.join();
    let mut ids = (1..)
        .map(|n| format!("meeting-{n}"))
        .filter(|id| !stored.contains_key(id));
    conference
        .labelled_ways_in()
        .map(|(point, label)| {
            let mut entry = Map::new();
            entry.insert("@type".to_owned(), json!("VirtualLocation"));
            entry.insert("uri".to_owned(), json!(point.uri.as_str()));
            if let Some(label) = label {
                entry.insert("name".to_owned(), json!(label));
            }
            let description = if Some(&point.uri) == join {
                conference.notes.as_ref().or(point.description.as_ref())
            } else {
                point.description.as_ref()
            };
            if let Some(description) = description {
                entry.insert("description".to_owned(), json!(description));
            }
            let features: Map<String, Value> = point
                .offered_features()
                .iter()
                .map(|feature| (feature.as_str().to_owned(), Value::Bool(true)))
                .collect();
            if !features.is_empty() {
                entry.insert("features".to_owned(), Value::Object(features));
            }
            let id = ids
                .next()
                .unwrap_or_else(|| unreachable!("the ids never run out"));
            (id, Value::Object(entry))
        })
        .collect()
}

/// The ids of the stored entries whose address `names`.
fn ids_where(stored: &Map<String, Value>, names: impl Fn(&ConferenceUri) -> bool) -> Vec<String> {
    stored
        .iter()
        .filter(|(_, entry)| {
            opt_str(entry, "uri")
                .and_then(|uri| ConferenceUri::parse(uri).ok())
                .is_some_and(|uri| names(&uri))
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// The pointers removing each location named `name`.
fn locations_named(raw: Option<&Value>, name: &str) -> Vec<(String, Value)> {
    raw.and_then(|raw| raw.get("locations"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, location)| opt_str(location, "name").map(str::trim) == Some(name.trim()))
        .map(|(id, _)| (format!("locations/{}", escape_pointer(id)), Value::Null))
        .collect()
}

/// The JSCalendar object the event was read from.
fn raw(base: &Event) -> Option<Value> {
    serde_json::from_str(base.raw_jscalendar.as_ref()?.as_str()).ok()
}

#[cfg(test)]
#[path = "calendar_conference_write_tests.rs"]
mod tests;
