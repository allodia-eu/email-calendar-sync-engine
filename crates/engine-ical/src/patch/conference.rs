//! What a conference edit writes into a stored `VEVENT`.
//!
//! The meeting an edit names is resolved against the projection of the stored document, the
//! same one the host read, so its key means here what it meant there. Taking a meeting off
//! removes every property that names it, its block in the notes and the location when that is
//! its link; putting one on writes its `CONFERENCE` lines, its block and, when the event has no
//! location, its link as the location ([`crate::conference_write`]).

use engine_core::calendar::{Event, without_text_block};
use engine_provider::{EventPatch, TextEdit};

use super::{
    super::{
        conference_write::{MEETING_PROPERTIES, attachment, property_uri},
        lines::Document,
        unfold::split_once_unquoted,
    },
    vevent::{Vevent, property_name},
};
use crate::error::IcalError;

/// The edits a conference edit implies. The notes and location are the text the event ends
/// with, and replace the patch's own edits of them, which they already include.
pub(super) struct ConferenceWrite {
    pub(super) description: Option<TextEdit>,
    pub(super) location: Option<TextEdit>,
    pub(super) remove: Vec<usize>,
    pub(super) insert: Vec<String>,
}

/// Plans `patch`'s conference edit on `vevent`, whose projection is `base`; `None` when the
/// patch has none.
///
/// # Errors
///
/// [`IcalError`] when the edit removes a meeting the event does not hold, asks the server for
/// a meeting of its own, or attaches one without a web link.
pub(super) fn plan(
    doc: &Document,
    vevent: &Vevent,
    base: &Event,
    patch: &EventPatch,
) -> Result<Option<ConferenceWrite>, IcalError> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(None);
    };
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
    let mut location_changed = false;
    let mut remove = Vec::new();

    if let Some(key) = edit.removes() {
        let conference = base
            .conference(key)
            .ok_or_else(|| IcalError::new("the event holds no meeting with that key"))?;
        notes = without_text_block(&notes, conference).into_owned();
        remove.extend(meeting_lines(doc, vevent, |uri| conference.joined_at(uri)));
        let is_link = conference
            .join()
            .is_some_and(|uri| uri.as_str() == location.trim());
        if !edits_location && is_link {
            location.clear();
            location_changed = true;
        }
    }
    let mut insert = Vec::new();
    if let Some(new) = edit.adds() {
        let meeting = attachment(&notes, new)?;
        // Lines the event already holds for the same ways in are replaced, never repeated.
        remove.extend(meeting_lines(doc, vevent, |uri| {
            meeting
                .conference
                .entry_points
                .iter()
                .any(|point| point.uri == *uri)
        }));
        notes = meeting.notes;
        insert = meeting.lines;
        if location.trim().is_empty() {
            location = meeting.join;
            location_changed = true;
        }
    }

    Ok(Some(ConferenceWrite {
        description: (notes != before).then(|| text_edit(notes)),
        location: location_changed.then(|| text_edit(location)),
        remove,
        insert,
    }))
}

/// The groups of `vevent`'s meeting properties whose URI `names`.
fn meeting_lines(
    doc: &Document,
    vevent: &Vevent,
    names: impl Fn(&engine_core::calendar::ConferenceUri) -> bool,
) -> Vec<usize> {
    vevent
        .own
        .iter()
        .copied()
        .filter(|&group| {
            let logical = doc.logical(group);
            let name = property_name(&logical);
            MEETING_PROPERTIES
                .iter()
                .any(|property| name.eq_ignore_ascii_case(property))
                && split_once_unquoted(&logical, ':')
                    .and_then(|(_, value)| property_uri(value))
                    .is_some_and(|uri| names(&uri))
        })
        .collect()
}

fn text_edit(text: String) -> TextEdit {
    if text.is_empty() {
        TextEdit::Clear
    } else {
        TextEdit::Set(text)
    }
}
