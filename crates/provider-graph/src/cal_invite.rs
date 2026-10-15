//! Microsoft Graph attendee array rendering and preservation.

use engine_core::calendar::Event;
use engine_provider::{EventDraft, InviteePatch, InviteeRole, ProviderError, ProviderResult};
use serde_json::{Map, Value, json};

use crate::{cal_normalize::RAW_EVENT_KEY, json::opt_str};

pub(crate) fn draft_attendees(draft: &EventDraft) -> ProviderResult<Option<Value>> {
    let Some(meeting) = &draft.meeting else {
        return Ok(None);
    };
    meeting
        .validate(Some(500))
        .map_err(|error| ProviderError::invalid_state(error.to_string()))?;
    Ok(Some(Value::Array(
        meeting
            .invitees()
            .iter()
            .map(|invitee| {
                let mut email = Map::new();
                email.insert(
                    "address".to_owned(),
                    json!(invitee.identity().address().as_str()),
                );
                if let Some(name) = invitee.identity().name() {
                    email.insert("name".to_owned(), json!(name));
                }
                json!({
                    "emailAddress": email,
                    "type": match invitee.role() {
                        InviteeRole::Required => "required",
                        InviteeRole::Optional => "optional",
                    },
                })
            })
            .collect(),
    )))
}

pub(crate) fn merge_attendees(base: &Event, edit: &InviteePatch) -> ProviderResult<Value> {
    let raw = base.extended.get(RAW_EVENT_KEY).ok_or_else(|| {
        ProviderError::invalid_state(
            "event has no preserved Graph payload; re-sync it before editing invitees",
        )
    })?;
    let organiser = raw
        .get("organizer")
        .and_then(|value| value.get("emailAddress"))
        .and_then(|value| opt_str(value, "address"));
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
        let index = attendees
            .iter()
            .position(|value| matches_email(value, address))
            .unwrap_or_else(|| {
                attendees.push(json!({ "emailAddress": {} }));
                attendees.len() - 1
            });
        let value = attendees[index]
            .as_object_mut()
            .ok_or_else(|| ProviderError::invalid_state("Graph attendee is not an object"))?;
        let email = value
            .entry("emailAddress")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                ProviderError::invalid_state("Graph attendee address is not an object")
            })?;
        email.insert("address".to_owned(), json!(address));
        if let Some(name) = invitee.identity().name() {
            email.insert("name".to_owned(), json!(name));
        }
        value.insert(
            "type".to_owned(),
            json!(match invitee.role() {
                InviteeRole::Required => "required",
                InviteeRole::Optional => "optional",
            }),
        );
    }
    if attendees.len() > 500 {
        return Err(ProviderError::invalid_state(format!(
            "provider accepts at most 500 invitees, got {}",
            attendees.len()
        )));
    }
    Ok(Value::Array(attendees))
}

fn matches_email(value: &Value, address: &str) -> bool {
    value
        .get("emailAddress")
        .and_then(|value| opt_str(value, "address"))
        .is_some_and(|value| value.eq_ignore_ascii_case(address))
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
