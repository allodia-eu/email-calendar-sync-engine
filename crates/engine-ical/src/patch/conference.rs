//! What a conference edit writes into a stored `VEVENT`.
//!
//! The meeting an edit names is resolved against the projection of the stored document, the
//! same one the host read, so its key means here what it meant there. Taking a meeting off
//! removes every property that names it, its block in the notes and the location when that is
//! its link; putting one on writes its `CONFERENCE` lines, its block and, when the event has no
//! location, its link as the location ([`crate::conference_write`]).

use engine_core::calendar::Event;
use engine_provider::{ConferenceEdit, EventPatch, TextEdit, meeting_text};

use super::{
    super::{
        conference_write::{MEETING_PROPERTIES, attached, conference_lines, property_uri},
        lines::Document,
        unfold::split_once_unquoted,
    },
    vevent::{Vevent, property_name},
};
use crate::error::IcalError;

/// The edits a conference edit implies. The notes and location are the text the event ends
/// with ([`meeting_text`]), and replace the patch's own edits of them, which they include.
pub(super) struct ConferenceWrite {
    pub(super) description: Option<TextEdit>,
    pub(super) location: Option<TextEdit>,
    pub(super) remove: Vec<usize>,
    pub(super) insert: Vec<String>,
}

/// Plans `patch`'s conference edit `edit` on `vevent`, whose projection is `base`.
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
    edit: &ConferenceEdit,
) -> Result<ConferenceWrite, IcalError> {
    let stored_location = base
        .locations
        .first()
        .and_then(|location| location.name.as_deref())
        .unwrap_or_default();
    let text = meeting_text(base, patch, stored_location)
        .map_err(|error| IcalError::new(error.to_string()))?
        .unwrap_or_else(|| unreachable!("a patch with a conference edit has a text plan"));
    let mut remove = Vec::new();
    if let Some(key) = edit.removes() {
        let conference = base
            .conference(key)
            .unwrap_or_else(|| unreachable!("meeting_text refused a key the event lacks"));
        remove.extend(meeting_lines(doc, vevent, |uri| conference.joined_at(uri)));
    }
    let mut insert = Vec::new();
    if let Some(new) = edit.adds() {
        let conference = &attached(new)?.conference;
        // Lines the event already holds for the same ways in are replaced, never repeated.
        remove.extend(meeting_lines(doc, vevent, |uri| {
            conference
                .entry_points
                .iter()
                .any(|point| point.uri == *uri)
        }));
        insert = conference_lines(conference);
    }
    Ok(ConferenceWrite {
        description: text.notes,
        location: text.location,
        remove,
        insert,
    })
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
