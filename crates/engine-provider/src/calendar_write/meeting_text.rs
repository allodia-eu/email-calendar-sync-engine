//! What a conference edit leaves in an event's notes and location, decided once for every
//! adapter that writes a meeting into them (`calendar-semantics.md`, "Writing a meeting").

use engine_core::calendar::{ConferenceOrigin, Event, with_text_block, without_text_block};

use super::{EventPatch, NewConference, TextEdit};
use crate::{ProviderError, ProviderResult};

/// The notes and location a conference edit leaves, each `None` when the edit leaves it as
/// the patch's own edit (or the stored value) has it. Each already includes the patch's own
/// edit of it, so an adapter writes this in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeetingText {
    /// The notes, when the edit changes them.
    pub notes: Option<TextEdit>,
    /// The location, when the edit changes it.
    pub location: Option<TextEdit>,
}

/// What `patch`'s conference edit leaves in the notes and location of `base`, whose location
/// is stored as `stored_location` (on Graph that includes the stand-in a server-made meeting
/// writes, which the projection leaves out); `None` when the patch has no conference edit.
///
/// Taking a meeting off removes every block in the notes that joins it, and the location when
/// that is still its link or a server-made meeting's stand-in; a location the patch sets is the
/// person's and is left alone. Putting one made elsewhere on replaces any block the notes
/// already carry for it, and makes its link the location when the event would have none. A
/// meeting the server makes ([`NewConference::Native`]) writes neither here: its server does.
///
/// # Errors
///
/// [`ProviderError::invalid_state`] when the edit removes a meeting `base` does not hold, or
/// attaches one without a web link to join it at.
pub fn meeting_text(
    base: &Event,
    patch: &EventPatch,
    stored_location: &str,
) -> ProviderResult<Option<MeetingText>> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(None);
    };
    let before = patch.description_after(base).unwrap_or_default();
    let mut notes = before.to_owned();
    let edits_location = patch.location_edit().is_some();
    let mut location = match patch.location_edit() {
        Some(TextEdit::Set(text)) => text.clone(),
        Some(TextEdit::Clear) => String::new(),
        None => stored_location.to_owned(),
    };
    let mut location_changed = false;

    if let Some(key) = edit.removes() {
        let conference = base.conference(key).ok_or_else(|| {
            ProviderError::invalid_state("the event holds no meeting with that key")
        })?;
        notes = without_text_block(&notes, conference).into_owned();
        let stand_in = matches!(conference.origin, ConferenceOrigin::Native { .. })
            && base.locations.is_empty();
        let is_link = conference
            .join()
            .is_some_and(|uri| uri.as_str() == location.trim());
        if !edits_location && !location.trim().is_empty() && (stand_in || is_link) {
            location.clear();
            location_changed = true;
        }
    }
    if let Some(NewConference::Attach(attached)) = edit.adds() {
        let conference = &attached.conference;
        let block = conference.text_block(&attached.text).ok_or_else(|| {
            ProviderError::invalid_state("a meeting to attach needs a web link to join it at")
        })?;
        notes = with_text_block(&without_text_block(&notes, conference), &block);
        if location.trim().is_empty() {
            location = conference
                .join()
                .map(|uri| uri.as_str().to_owned())
                .unwrap_or_default();
            location_changed = true;
        }
    }

    let changed = notes != before;
    Ok(Some(MeetingText {
        notes: changed.then(|| text_edit(notes)),
        location: location_changed.then(|| text_edit(location)),
    }))
}

fn text_edit(text: String) -> TextEdit {
    if text.is_empty() {
        TextEdit::Clear
    } else {
        TextEdit::Set(text)
    }
}

#[cfg(test)]
#[path = "meeting_text_tests.rs"]
mod tests;
