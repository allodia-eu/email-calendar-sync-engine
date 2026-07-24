//! Google People create/update request bodies.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::contact::{
    Anniversary, ContactAddress, ContactCard, ContactDraft, ContactEmail, ContactField,
    ContactName, ContactNickname, ContactNote, ContactPatch, ContactPhone, ContactProperty,
    ContactRelation, ContactResource, FieldPatch, Organization, PropertyId, Title,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::error::GoogleError;

pub(crate) fn create_body(draft: &ContactDraft) -> Result<Vec<u8>, GoogleError> {
    if draft.card.kind != engine_core::contact::ContactKind::Individual {
        return Err(GoogleError::protocol(
            "Google People owned contacts support only individual cards",
        ));
    }
    Ok(serde_json::to_vec(&fields(&draft.card)?)?)
}

pub(crate) fn patch_body(base: &ContactCard, patch: &ContactPatch) -> Result<Vec<u8>, GoogleError> {
    validate_kind(patch)?;
    let mut body = base
        .raw_provider_json
        .as_ref()
        .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw.as_str()).ok())
        .unwrap_or_default();
    let mut organizations = base.organizations.clone();
    let mut titles = base.titles.clone();
    for (field, edit) in &patch.fields {
        match field {
            ContactField::Organizations => apply_map(edit, &mut organizations)?,
            ContactField::Titles => apply_map(edit, &mut titles)?,
            _ => {
                let key = field_name(*field).ok_or_else(|| {
                    GoogleError::protocol(format!("unsupported Google contact field {field:?}"))
                })?;
                let value = match edit {
                    FieldPatch::Set(value) => field_value(*field, value)?,
                    FieldPatch::Clear => Value::Array(Vec::new()),
                };
                body.insert(key.into(), value);
            }
        }
    }
    if patch.fields.contains_key(&ContactField::Organizations)
        || patch.fields.contains_key(&ContactField::Titles)
    {
        body.insert(
            "organizations".into(),
            organization_values(&organizations, &titles),
        );
    }
    if let Some(etag) = base.revisions.etag.as_ref() {
        body.insert("etag".into(), json!(etag.as_str()));
    }
    Ok(serde_json::to_vec(&body)?)
}

pub(crate) fn update_fields(patch: &ContactPatch) -> Result<String, GoogleError> {
    validate_kind(patch)?;
    let mut fields = BTreeSet::new();
    for field in patch.fields.keys().copied() {
        fields.insert(field_name(field).ok_or_else(|| {
            GoogleError::protocol(format!("unsupported Google contact field {field:?}"))
        })?);
    }
    Ok(fields.into_iter().collect::<Vec<_>>().join(","))
}

fn fields(card: &ContactCard) -> Result<Value, GoogleError> {
    let mut body = Map::new();
    if let Some(name) = &card.name {
        body.insert("names".into(), name_values(name));
    }
    body.insert("nicknames".into(), nickname_values(&card.nicknames));
    body.insert("emailAddresses".into(), email_values(&card.emails));
    body.insert("phoneNumbers".into(), phone_values(&card.phones));
    body.insert("addresses".into(), address_values(&card.addresses));
    body.insert(
        "organizations".into(),
        organization_values(&card.organizations, &card.titles),
    );
    body.insert("birthdays".into(), anniversary_values(&card.anniversaries)?);
    body.insert("biographies".into(), note_values(&card.notes));
    body.insert("urls".into(), resource_values(&card.urls));
    body.insert("relations".into(), relation_values(&card.relations));
    body.insert("userDefined".into(), keyword_values(&card.keywords));
    Ok(Value::Object(body))
}

fn field_name(field: ContactField) -> Option<&'static str> {
    match field {
        ContactField::Name => Some("names"),
        ContactField::Nicknames => Some("nicknames"),
        ContactField::Emails => Some("emailAddresses"),
        ContactField::Phones => Some("phoneNumbers"),
        ContactField::Addresses => Some("addresses"),
        ContactField::Organizations | ContactField::Titles => Some("organizations"),
        ContactField::Anniversaries => Some("birthdays"),
        ContactField::Notes => Some("biographies"),
        ContactField::Urls => Some("urls"),
        ContactField::Relations => Some("relations"),
        ContactField::Keywords => Some("userDefined"),
        _ => None,
    }
}

fn field_value(field: ContactField, value: &Value) -> Result<Value, GoogleError> {
    Ok(match field {
        ContactField::Name => name_values(&decode(value)?),
        ContactField::Nicknames => nickname_values(&decode(value)?),
        ContactField::Emails => email_values(&decode(value)?),
        ContactField::Phones => phone_values(&decode(value)?),
        ContactField::Addresses => address_values(&decode(value)?),
        ContactField::Anniversaries => anniversary_values(&decode(value)?)?,
        ContactField::Notes => note_values(&decode(value)?),
        ContactField::Urls => resource_values(&decode(value)?),
        ContactField::Relations => relation_values(&decode(value)?),
        ContactField::Keywords => keyword_values(&decode(value)?),
        _ => {
            return Err(GoogleError::protocol(format!(
                "unsupported Google contact field {field:?}"
            )));
        }
    })
}

fn name_values(name: &ContactName) -> Value {
    let mut item = Map::new();
    item.insert("displayName".into(), json!(name.display()));
    for component in &name.components {
        let key = match component.kind {
            engine_core::contact::NameComponentKind::Prefix => "honorificPrefix",
            engine_core::contact::NameComponentKind::Given => "givenName",
            engine_core::contact::NameComponentKind::Middle => "middleName",
            engine_core::contact::NameComponentKind::Surname => "familyName",
            engine_core::contact::NameComponentKind::Suffix => "honorificSuffix",
            _ => continue,
        };
        item.insert(key.into(), json!(component.value));
    }
    Value::Array(vec![Value::Object(item)])
}

fn property_metadata<T>(property: &ContactProperty<T>) -> Value {
    let context = property
        .contexts
        .iter()
        .next()
        .map(|value| if value == "private" { "home" } else { value });
    json!({
        "type": context,
        "metadata": {"primary": property.preference == Some(1)}
    })
}

fn property_item<T>(property: &ContactProperty<T>, key: &str, value: &str) -> Value {
    let mut item = property_metadata(property);
    item[key] = json!(value);
    item
}

fn nickname_values(values: &BTreeMap<PropertyId, ContactProperty<ContactNickname>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| property_item(entry, "value", &entry.value.name))
            .collect(),
    )
}

fn email_values(values: &BTreeMap<PropertyId, ContactProperty<ContactEmail>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| property_item(entry, "value", &entry.value.address))
            .collect(),
    )
}

fn phone_values(values: &BTreeMap<PropertyId, ContactProperty<ContactPhone>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| {
                let mut item = property_item(entry, "value", &entry.value.number);
                if entry.value.features.contains("mobile") {
                    item["type"] = json!("mobile");
                }
                item
            })
            .collect(),
    )
}

fn address_values(values: &BTreeMap<PropertyId, ContactProperty<ContactAddress>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| {
                let mut item = property_metadata(entry);
                let component = |name: &str| {
                    entry
                        .value
                        .components
                        .get(name)
                        .and_then(|values| values.first())
                };
                for (key, value) in [
                    ("formattedValue", entry.value.full.as_ref()),
                    ("streetAddress", component("street")),
                    ("city", component("locality")),
                    ("region", component("region")),
                    ("postalCode", component("postcode")),
                    ("country", component("country")),
                    ("countryCode", entry.value.country_code.as_ref()),
                ] {
                    if let Some(value) = value {
                        item[key] = json!(value);
                    }
                }
                item
            })
            .collect(),
    )
}

fn organization_values(
    organizations: &BTreeMap<PropertyId, ContactProperty<Organization>>,
    titles: &BTreeMap<PropertyId, ContactProperty<Title>>,
) -> Value {
    let mut items = Vec::new();
    let mut used_titles = BTreeSet::new();
    for (id, organization) in organizations {
        let mut item = property_metadata(organization);
        item["name"] = json!(organization.value.name);
        if let Some(unit) = organization.value.units.first() {
            item["department"] = json!(unit.name);
        }
        if let Some((title_id, title)) = titles
            .iter()
            .find(|(_, title)| title.value.organization_id.as_ref() == Some(id))
            .or_else(|| {
                (organizations.len() == 1)
                    .then(|| titles.iter().next())
                    .flatten()
            })
        {
            item["title"] = json!(title.value.name);
            used_titles.insert(title_id.clone());
        }
        items.push(item);
    }
    for (id, title) in titles {
        if !used_titles.contains(id) {
            items.push(json!({"title": title.value.name}));
        }
    }
    Value::Array(items)
}

fn anniversary_values(
    values: &BTreeMap<PropertyId, ContactProperty<Anniversary>>,
) -> Result<Value, GoogleError> {
    let mut items = Vec::new();
    for entry in values.values() {
        if !matches!(entry.value.kind.as_deref(), None | Some("birth")) {
            return Err(GoogleError::protocol(
                "Google contact writes support only birth anniversaries",
            ));
        }
        let parts = entry
            .value
            .date
            .split('-')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| GoogleError::protocol("Google birthday must be YYYY-MM-DD"))?;
        if parts.len() != 3 {
            return Err(GoogleError::protocol("Google birthday must be YYYY-MM-DD"));
        }
        items.push(json!({
            "date": {"year": parts[0], "month": parts[1], "day": parts[2]},
            "metadata": {"primary": entry.preference == Some(1)}
        }));
    }
    Ok(Value::Array(items))
}

fn note_values(values: &BTreeMap<PropertyId, ContactProperty<ContactNote>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| property_item(entry, "value", &entry.value.note))
            .collect(),
    )
}

fn resource_values(values: &BTreeMap<PropertyId, ContactProperty<ContactResource>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| property_item(entry, "value", &entry.value.uri))
            .collect(),
    )
}

fn relation_values(values: &BTreeMap<PropertyId, ContactProperty<ContactRelation>>) -> Value {
    Value::Array(
        values
            .values()
            .map(|entry| {
                json!({
                    "person": entry.value.uid.as_ref().or(entry.value.uri.as_ref()),
                    "type": entry.value.relation.iter().next()
                })
            })
            .collect(),
    )
}

fn keyword_values(values: &BTreeSet<String>) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| json!({"key": "category", "value": value}))
            .collect(),
    )
}

fn apply_map<T: Default + DeserializeOwned>(
    edit: &FieldPatch<Value>,
    target: &mut BTreeMap<PropertyId, ContactProperty<T>>,
) -> Result<(), GoogleError> {
    *target = match edit {
        FieldPatch::Set(value) => decode(value)?,
        FieldPatch::Clear => BTreeMap::new(),
    };
    Ok(())
}

fn validate_kind(patch: &ContactPatch) -> Result<(), GoogleError> {
    if matches!(
        patch.kind,
        Some(FieldPatch::Set(
            ref kind
        )) if *kind != engine_core::contact::ContactKind::Individual
    ) || matches!(patch.kind, Some(FieldPatch::Clear))
    {
        return Err(GoogleError::protocol(
            "Google People owned contacts support only individual cards",
        ));
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, GoogleError> {
    serde_json::from_value(value.clone()).map_err(GoogleError::from)
}

#[cfg(test)]
#[path = "contact_write_tests.rs"]
mod tests;
