//! Google Calendar attendee array rendering and preservation.

use engine_core::calendar::Event;
use engine_provider::{EventDraft, InviteePatch, InviteeRole, ProviderError, ProviderResult};
use serde_json::{Map, Value, json};

use crate::{cal_normalize::RAW_EVENT_KEY, json::opt_str};

pub(crate) fn draft_attendees(draft: &EventDraft) -> ProviderResult<Option<Value>> {
    let Some(meeting) = &draft.meeting else {
        return Ok(None);
    };
    meeting
        .validate(None)
        .map_err(|error| ProviderError::invalid_state(error.to_string()))?;
    Ok(Some(Value::Array(
        meeting
            .invitees()
            .iter()
            .map(|invitee| {
                let mut attendee = Map::new();
                attendee.insert(
                    "email".to_owned(),
                    json!(invitee.identity().address().as_str()),
                );
                if let Some(name) = invitee.identity().name() {
                    attendee.insert("displayName".to_owned(), json!(name));
                }
                if invitee.role() == InviteeRole::Optional {
                    attendee.insert("optional".to_owned(), json!(true));
                }
                attendee.insert("responseStatus".to_owned(), json!("needsAction"));
                Value::Object(attendee)
            })
            .collect(),
    )))
}

pub(crate) fn merge_attendees(base: &Event, edit: &InviteePatch) -> ProviderResult<Value> {
    let raw = base.extended.get(RAW_EVENT_KEY).ok_or_else(|| {
        ProviderError::invalid_state(
            "event has no preserved Google payload; re-sync it before editing invitees",
        )
    })?;
    if raw
        .get("attendeesOmitted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(ProviderError::invalid_state(
            "Google omitted part of the attendee list; fetch the complete event before editing invitees",
        ));
    }
    let organiser = raw
        .get("organizer")
        .and_then(|value| opt_str(value, "email"));
    let mut attendees = raw
        .get("attendees")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for address in edit.removals() {
        reject_organiser(organiser, address.as_str())?;
        attendees.retain(|value| !matches_email(value, address.as_str()));
    }
    for invitee in edit.upserts() {
        let address = invitee.identity().address().as_str();
        reject_organiser(organiser, address)?;
        if let Some(Value::Object(value)) = attendees
            .iter_mut()
            .find(|value| matches_email(value, address))
        {
            value.insert("email".to_owned(), json!(address));
            if let Some(name) = invitee.identity().name() {
                value.insert("displayName".to_owned(), json!(name));
            }
            match invitee.role() {
                InviteeRole::Required => {
                    value.remove("optional");
                }
                InviteeRole::Optional => {
                    value.insert("optional".to_owned(), json!(true));
                }
            }
        } else {
            let mut value = Map::new();
            value.insert("email".to_owned(), json!(address));
            if let Some(name) = invitee.identity().name() {
                value.insert("displayName".to_owned(), json!(name));
            }
            if invitee.role() == InviteeRole::Optional {
                value.insert("optional".to_owned(), json!(true));
            }
            value.insert("responseStatus".to_owned(), json!("needsAction"));
            attendees.push(Value::Object(value));
        }
    }
    Ok(Value::Array(attendees))
}

fn matches_email(value: &Value, address: &str) -> bool {
    opt_str(value, "email").is_some_and(|value| value.eq_ignore_ascii_case(address))
}

fn reject_organiser(organiser: Option<&str>, address: &str) -> ProviderResult<()> {
    if organiser.is_some_and(|value| value.eq_ignore_ascii_case(address)) {
        Err(ProviderError::invalid_state(
            "the organiser cannot be edited as an invitee",
        ))
    } else {
        Ok(())
    }
}
