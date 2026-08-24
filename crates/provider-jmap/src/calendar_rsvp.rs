//! The RSVP: one `CalendarEvent/set` `update` of *my* participant's `participationStatus`.
//!
//! Its own module because it is the only write here that has to resolve an id the
//! projection cannot supply. `Event::participants` is a `Vec` — the JSCalendar map keys are
//! gone by the time the engine sees it — and the key is exactly what a JSON-pointer patch
//! addresses. So this reaches back into the preserved `raw_jscalendar`, which no other
//! write on this transport does except a location rename.

use engine_core::{calendar::Event, scheduling::addresses_match, version::RevisionTokens};
use engine_provider::{EventRsvp, EventWriteReceipt};
use serde_json::{Map, Value, json};

use crate::{
    calendar::participant_address,
    calendar_write::{escape_pointer, set_error},
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// Answers an invitation: one `CalendarEvent/set` `update` of *my* participant's
/// `participationStatus`.
///
/// A single JSON pointer, `participants/<my id>/participationStatus`, so every other
/// participant — and every property of mine the engine does not model — is left exactly as
/// the server holds it.
///
/// # Scheduling is opt-in, and forgetting it is silent
///
/// Changing the status is **not** what tells the organizer. `sendSchedulingMessages`
/// (default **`false`**) is, and a `/set` that omits it is stored and goes nowhere: the
/// attendee's own calendar agrees, the organizer is never told, and nothing on either side
/// reports a failure. That is exactly what this adapter did until it was fixed — and the
/// live test that was supposed to catch it instead recorded the absence as *server*
/// behaviour, because it only ever sent the one request shape the adapter builds (#102).
///
/// It is also why JMAP is the one server-scheduled transport that **can** honour
/// [`EventRsvp::notify_organizer`](engine_provider::EventRsvp::notify_organizer): scheduling
/// is a per-request choice here, not something the server does on its own the way an RFC
/// 6638 CalDAV server does.
///
/// **The participant id is the map key, not the address**, and it lives only in the
/// preserved `raw_jscalendar` — the same reason the location edit consults it. So this
/// resolves the key by matching the participant's calendar address (in either JSCalendar
/// version — [`participant_address`]) against the answering address, using the engine's one
/// `addresses_match` normalization rather than a second copy: an alias invitation answers as
/// the alias, and `MAILTO:Info@…` is the same participant as `mailto:info@…`.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the event carries no preserved JSCalendar (never
/// synced from this transport) or has no participant at that address — you cannot answer an
/// invitation you are not on. Returns [`JmapError`] on a transport/method failure, or
/// [`JmapError::Set`] when the server rejects the update (or silently drops it — a conflict,
/// never a false success).
pub(crate) async fn rsvp_event(
    executor: &dyn Executor,
    calendar_account: &str,
    base: &Event,
    rsvp: &EventRsvp,
) -> Result<EventWriteReceipt, JmapError> {
    let target = rsvp.event.as_str();
    let participant = my_participant_id(base, &rsvp.attendee)?;
    let patch = json!({
        format!("participants/{}/participationStatus", escape_pointer(&participant)):
            rsvp.response.status().as_str(),
    });
    let mut update = Map::new();
    update.insert(target.to_owned(), patch);
    // Without this the answer is stored and *goes nowhere*: the argument defaults to `false`,
    // so the server writes the status and schedules no `REPLY` at all. Sent explicitly in both
    // directions rather than omitted for the quiet case, so the request states the user's
    // choice instead of relying on a default that could move with the draft.
    let args = json!({
        "accountId": calendar_account,
        "update": update,
        "sendSchedulingMessages": rsvp.notify_organizer,
    });

    let mut req = Request::new([capability::CORE, capability::CALENDARS]);
    let call = req.invoke("CalendarEvent/set", args);
    let resp = executor.execute(&req).await?;
    let result = resp.result(&call)?;

    if let Some(error_type) = set_error(result, "notUpdated", target) {
        return Err(JmapError::set(target, error_type));
    }
    // As in `patch_event`: a target mentioned in neither map was silently dropped, which is
    // a conflict rather than a false success.
    if result.get("updated").and_then(|u| u.get(target)).is_none() {
        return Err(JmapError::set(target, "notFound"));
    }
    Ok(EventWriteReceipt::new(
        rsvp.event.clone(),
        rsvp.uid.clone(),
        RevisionTokens::none(),
    ))
}

/// The `participants` map key whose `calendarAddress` is `me`, from the preserved
/// JSCalendar.
///
/// The projection cannot answer this: [`Event::participants`](engine_core::calendar::Event)
/// is a `Vec` that has already thrown the map keys away, and the key is what the patch
/// pointer needs.
pub(crate) fn my_participant_id(base: &Event, me: &str) -> Result<String, JmapError> {
    let raw = base.raw_jscalendar.as_ref().ok_or_else(|| {
        JmapError::protocol(
            "event has no preserved JSCalendar, so its participant ids are unknown; it was not \
             synced from this provider. Re-sync the calendar before answering",
        )
    })?;
    let value: Value = serde_json::from_str(raw.as_str())
        .map_err(|e| JmapError::protocol(format!("preserved JSCalendar is not JSON: {e}")))?;
    value
        .get("participants")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .find(|(_, participant)| {
            participant_address(participant).is_some_and(|addr| addresses_match(addr, me))
        })
        .map(|(id, _)| id.clone())
        .ok_or_else(|| {
            JmapError::protocol(
                "the event has no participant at the answering address; you cannot answer an \
                 invitation you are not on",
            )
        })
}
