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
    Conference, ConferenceService, ConferenceText, Event, with_text_block,
};
use engine_provider::{
    EventDraft, EventPatch, NewConference, ProviderError, ProviderResult, TextEdit, meeting_text,
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
/// location the same patch already sets ([`meeting_text`]).
pub(crate) fn apply_to_patch(
    base: &Event,
    patch: &EventPatch,
    body: &mut Map<String, Value>,
) -> ProviderResult<()> {
    let Some(edit) = patch.conference_edit() else {
        return Ok(());
    };
    if let Some(NewConference::Native(service)) = edit.adds() {
        ask_for(service, body)?;
    }
    let Some(text) = meeting_text(base, patch, &stored_location(base))? else {
        return Ok(());
    };
    // An unchanged body is not written: a text write flattens the HTML Exchange holds.
    if let Some(notes) = &text.notes {
        body.insert("body".to_owned(), text_body(text_value(notes)));
    }
    if let Some(location) = &text.location {
        body.insert(
            "location".to_owned(),
            json!({ "displayName": text_value(location) }),
        );
    }
    Ok(())
}

fn text_value(edit: &TextEdit) -> &str {
    match edit {
        TextEdit::Set(text) => text,
        TextEdit::Clear => "",
    }
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
