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

use engine_core::calendar::{
    Conference, ConferenceUri, Event, with_text_block, without_text_block,
};
use engine_provider::{EventDraft, EventPatch, NewConference, TextEdit};
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

/// Plans `patch`'s conference edit against `base`; `None` when the patch has none.
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
    let before = patch.description_after(base).unwrap_or_default();
    let mut notes = before.to_owned();
    // A location the patch sets is the person's; only the stored one is the meeting's to clear.
    let edits_location = patch.location_edit().is_some();
    let mut location = match patch.location_edit() {
        Some(TextEdit::Set(text)) => text.clone(),
        Some(TextEdit::Clear) => String::new(),
        None => base
            .locations
            .first()
            .and_then(|location| location.name.clone())
            .unwrap_or_default(),
    };
    let mut location_write = None;
    let mut removed = Map::new();

    if let Some(key) = edit.removes() {
        let conference = base
            .conference(key)
            .ok_or_else(|| JmapError::protocol("the event holds no meeting with that key"))?;
        notes = without_text_block(&notes, conference).into_owned();
        for id in ids_where(&stored, |uri| conference.joined_at(uri)) {
            removed.insert(id, Value::Null);
        }
        if !edits_location && join_matches(conference, &location) {
            location.clear();
            location_write = Some(location_ids(raw.as_ref(), conference));
        }
    }
    let mut added = Map::new();
    if let Some(new) = edit.adds() {
        let (conference, block) = attached(new)?;
        // Entries the event already holds for the same ways in are replaced, never repeated.
        for id in ids_where(&stored, |uri| {
            conference
                .entry_points
                .iter()
                .any(|point| point.uri == *uri)
        }) {
            removed.insert(id, Value::Null);
        }
        notes = with_text_block(&without_text_block(&notes, conference), &block);
        added = entries(conference, &stored);
        if location.trim().is_empty() {
            location_write = Some(location_edit(base, &TextEdit::Set(join(conference))));
        }
    }

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
    let changed = notes != before;
    let description = if notes.is_empty() {
        TextEdit::Clear
    } else {
        TextEdit::Set(notes)
    };
    Ok(Some(ConferenceWrite {
        description: changed.then_some(description),
        location: location_write,
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

fn join_matches(conference: &Conference, location: &str) -> bool {
    conference
        .join()
        .is_some_and(|uri| uri.as_str() == location.trim())
}

/// One `VirtualLocation` per way in, the join link first, at ids `stored` does not use. The
/// meeting's name labels its join link, which is where the reader takes it from.
fn entries(conference: &Conference, stored: &Map<String, Value>) -> Map<String, Value> {
    let join = conference.join();
    let mut points: Vec<_> = conference.entry_points.iter().collect();
    points.sort_by_key(|point| Some(&point.uri) != join);
    let mut ids = (1..)
        .map(|n| format!("meeting-{n}"))
        .filter(|id| !stored.contains_key(id));
    points
        .into_iter()
        .map(|point| {
            let mut entry = Map::new();
            entry.insert("@type".to_owned(), json!("VirtualLocation"));
            entry.insert("uri".to_owned(), json!(point.uri.as_str()));
            let label = if Some(&point.uri) == join {
                conference.label.as_ref().or(point.label.as_ref())
            } else {
                point.label.as_ref()
            };
            if let Some(label) = label {
                entry.insert("name".to_owned(), json!(label));
            }
            if let Some(description) = &point.description {
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

/// The pointers removing each location whose name is `conference`'s join link.
fn location_ids(raw: Option<&Value>, conference: &Conference) -> Vec<(String, Value)> {
    raw.and_then(|raw| raw.get("locations"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, location)| {
            opt_str(location, "name").is_some_and(|name| join_matches(conference, name))
        })
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
