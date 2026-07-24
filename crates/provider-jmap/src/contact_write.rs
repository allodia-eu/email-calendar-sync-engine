//! JMAP ContactCard create objects and PatchObjects.

use engine_core::contact::{ContactCard, ContactField, ContactKind, ContactPatch, FieldPatch};
use serde_json::{Map, Value, json};

use crate::{contact_write_fields, error::JmapError};

pub(crate) fn writable_object(card: &ContactCard) -> Map<String, Value> {
    if let Some(raw) = &card.raw_jscontact
        && let Ok(mut value) = serde_json::from_str::<Map<String, Value>>(raw.as_str())
    {
        value.remove("id");
        return value;
    }
    contact_write_fields::card_object(card)
}

pub(crate) fn patch_object(patch: &ContactPatch) -> Result<Map<String, Value>, JmapError> {
    let mut object = Map::new();
    if let Some(kind) = &patch.kind {
        object.insert(
            "kind".into(),
            match kind {
                FieldPatch::Set(kind) => json!(kind_name(kind)),
                FieldPatch::Clear => Value::Null,
            },
        );
    }
    for (field, value) in &patch.fields {
        let name = field_name(*field)
            .ok_or_else(|| JmapError::protocol(format!("unsupported contact field {field:?}")))?;
        object.insert(
            name.into(),
            match value {
                FieldPatch::Set(value) => contact_write_fields::field_value(*field, value)?,
                FieldPatch::Clear => Value::Null,
            },
        );
    }
    Ok(object)
}

fn field_name(field: ContactField) -> Option<&'static str> {
    Some(match field {
        ContactField::Name => "name",
        ContactField::Nicknames => "nicknames",
        ContactField::Emails => "emails",
        ContactField::Phones => "phones",
        ContactField::Addresses => "addresses",
        ContactField::Organizations => "organizations",
        ContactField::Titles => "titles",
        ContactField::Anniversaries => "anniversaries",
        ContactField::Notes => "notes",
        ContactField::Urls => "links",
        ContactField::OnlineServices => "onlineServices",
        ContactField::Relations => "relatedTo",
        ContactField::Languages => "preferredLanguages",
        ContactField::PersonalInfo => "personalInfo",
        ContactField::Calendars => "calendars",
        ContactField::SchedulingAddresses => "schedulingAddresses",
        ContactField::CryptoKeys => "cryptoKeys",
        ContactField::Directories => "directories",
        ContactField::Keywords => "keywords",
        ContactField::Kind | ContactField::TimeZone => return None,
    })
}

pub(super) fn kind_name(kind: &ContactKind) -> &str {
    match kind {
        ContactKind::Individual => "individual",
        ContactKind::Organization => "org",
        ContactKind::Group => "group",
        ContactKind::Location => "location",
        ContactKind::Device => "device",
        ContactKind::Application => "application",
        ContactKind::Other(value) => value,
    }
}

#[cfg(test)]
#[path = "contact_write_tests.rs"]
mod tests;
