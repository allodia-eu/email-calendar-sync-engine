//! Putting a video meeting on a Graph event, and taking one off. Measured on a work tenant:
//!
//! - **Teams** is `isOnlineMeeting: true` with `onlineMeetingProvider: teamsForBusiness`, on a
//!   create or a patch. Exchange makes the meeting, appends Teams' block to the body and sets its
//!   stand-in location, and the answer already carries the `joinUrl`.
//! - **Taking Teams off** is writing the body without Teams' block, and clearing the stand-in
//!   location. Graph accepts `isOnlineMeeting: false` and ignores it. Putting Teams back on makes a
//!   new meeting.
//! - **A meeting made elsewhere** has no field on Graph, so it goes into the notes as a block of
//!   ours, and into the location when the event has none.
//! - **Taking one off** removes its block, and the location when that is still its link.
//!
//! The body is written as text, as every Graph notes edit is (`cal_write`).

use engine_core::calendar::{
    Conference, ConferenceOrigin, ConferenceService, ConferenceText, Event, with_text_block,
    without_text_block,
};
use engine_provider::{
    EventDraft, EventPatch, NewConference, ProviderError, ProviderResult, TextEdit,
};
use serde_json::{Map, Value, json};

use crate::{cal_normalize::RAW_EVENT_KEY, cal_write::text_body, json::opt_str};

/// Adds the meeting `draft` puts on the event to the create's `body`.
pub(crate) fn apply_to_create(
    draft: &EventDraft,
    body: &mut Map<String, Value>,
) -> ProviderResult<()> {
    match &draft.conference {
        None => Ok(()),
        Some(NewConference::Native(service)) => ask_for(service, body),
        Some(NewConference::Attach(attached)) => {
            let (conference, text) = (&attached.conference, &attached.text);
            let (join, block) = block(conference, text)?;
            let notes = with_text_block(draft.description.as_deref().unwrap_or_default(), &block);
            body.insert("body".to_owned(), text_body(&notes));
            if draft.location.is_none() {
                body.insert("location".to_owned(), json!({ "displayName": join }));
            }
            Ok(())
        }
    }
}

/// Adds what `patch`'s conference edit writes to the patch's `body`, over the notes and
/// location the same patch already sets.
pub(crate) fn apply_to_patch(
    base: &Event,
    patch: &EventPatch,
    body: &mut Map<String, Value>,
) -> ProviderResult<()> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(());
    };
    let before = patch.description_after(base).unwrap_or_default();
    let mut notes = before.to_owned();
    // A location the patch sets is the person's; only the stored one is the meeting's to clear.
    let edits_location = patch.location_edit().is_some();
    let mut location = match patch.location_edit() {
        Some(TextEdit::Set(text)) => text.clone(),
        Some(TextEdit::Clear) => String::new(),
        None => stored_location(base),
    };
    let mut location_changed = false;
    if let Some(key) = edit.removes() {
        let conference = base.conference(key).ok_or_else(not_held)?;
        notes = without_text_block(&notes, conference).into_owned();
        let stand_in = matches!(conference.origin, ConferenceOrigin::Native { .. })
            && base.locations.is_empty();
        let is_link = conference
            .join()
            .is_some_and(|uri| uri.as_str() == location);
        if !edits_location && !location.is_empty() && (stand_in || is_link) {
            location.clear();
            location_changed = true;
        }
    }
    match edit.adds() {
        None => {}
        Some(NewConference::Native(service)) => ask_for(service, body)?,
        Some(NewConference::Attach(attached)) => {
            let (conference, text) = (&attached.conference, &attached.text);
            let (join, block) = block(conference, text)?;
            // A block the notes already carry for this meeting is replaced, never repeated.
            notes = with_text_block(&without_text_block(&notes, conference), &block);
            if location.is_empty() {
                location = join;
                location_changed = true;
            }
        }
    }
    // An unchanged body is not written: a text write flattens the HTML Exchange holds.
    if notes != before {
        body.insert("body".to_owned(), text_body(&notes));
    }
    if location_changed {
        body.insert("location".to_owned(), json!({ "displayName": location }));
    }
    Ok(())
}

/// Asks Exchange for a meeting on `service`; it makes Teams meetings only.
fn ask_for(service: &ConferenceService, body: &mut Map<String, Value>) -> ProviderResult<()> {
    if *service != ConferenceService::Teams {
        return Err(ProviderError::invalid_state(format!(
            "an Exchange calendar makes Teams meetings, not {service:?}"
        )));
    }
    body.insert("isOnlineMeeting".to_owned(), json!(true));
    body.insert(
        "onlineMeetingProvider".to_owned(),
        json!("teamsForBusiness"),
    );
    Ok(())
}

/// A meeting's join link and the block of ours that carries it.
fn block(conference: &Conference, text: &ConferenceText) -> ProviderResult<(String, String)> {
    let join = conference.join().ok_or_else(|| {
        ProviderError::invalid_state("a meeting to attach needs a web link to join it at")
    })?;
    let block = conference
        .text_block(text)
        .unwrap_or_else(|| unreachable!("a meeting with a join link has a block"));
    Ok((join.as_str().to_owned(), block))
}

/// The refusal of a removal naming a meeting the event does not hold.
fn not_held() -> ProviderError {
    ProviderError::invalid_state("the event holds no meeting with that key")
}

/// The location as Graph holds it, Exchange's stand-in included, which the projection leaves
/// out.
fn stored_location(base: &Event) -> String {
    base.extended
        .get(RAW_EVENT_KEY)
        .and_then(|raw| raw.get("location"))
        .and_then(|location| opt_str(location, "displayName"))
        .unwrap_or_default()
        .to_owned()
}
