//! Encoding normalized contact intent as RFC 9553 JSContact values.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::contact::{
    Anniversary, ContactAddress, ContactCard, ContactEmail, ContactField, ContactLanguage,
    ContactMember, ContactName, ContactNickname, ContactNote, ContactOnlineService, ContactPhone,
    ContactProperty, ContactRelation, ContactResource, Organization, PersonalInfo, PropertyId,
    Title,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};

use crate::error::JmapError;

pub(super) fn card_object(card: &ContactCard) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("@type".into(), json!("Card"));
    object.insert("version".into(), json!("1.0"));
    object.insert(
        "kind".into(),
        json!(super::contact_write::kind_name(&card.kind)),
    );
    if let Some(uid) = &card.uid {
        object.insert("uid".into(), json!(uid));
    }
    if let Some(name) = &card.name {
        object.insert("name".into(), name_value(name));
    }
    insert_property(&mut object, "nicknames", &card.nicknames);
    insert_property(&mut object, "emails", &card.emails);
    insert_property(&mut object, "phones", &card.phones);
    if !card.addresses.is_empty() {
        object.insert("addresses".into(), address_values(&card.addresses));
    }
    insert_property(&mut object, "organizations", &card.organizations);
    insert_property(&mut object, "titles", &card.titles);
    insert_property(&mut object, "anniversaries", &card.anniversaries);
    insert_property(&mut object, "notes", &card.notes);
    insert_property(&mut object, "links", &card.urls);
    insert_property(&mut object, "media", &card.media);
    insert_property(&mut object, "onlineServices", &card.online_services);
    if !card.relations.is_empty() {
        object.insert("relatedTo".into(), relation_values(&card.relations));
    }
    if !card.languages.is_empty() {
        object.insert(
            "preferredLanguages".into(),
            language_values(&card.languages),
        );
    }
    insert_members(&mut object, &card.members);
    insert_property(&mut object, "personalInfo", &card.personal_info);
    insert_property(&mut object, "calendars", &card.calendars);
    insert_property(
        &mut object,
        "schedulingAddresses",
        &card.scheduling_addresses,
    );
    insert_property(&mut object, "cryptoKeys", &card.crypto_keys);
    insert_property(&mut object, "directories", &card.directories);
    if !card.keywords.is_empty() {
        object.insert("keywords".into(), bool_set(&card.keywords));
    }
    if let Some(created) = card.created {
        object.insert("created".into(), json!(created.to_string()));
    }
    if let Some(updated) = card.updated {
        object.insert("updated".into(), json!(updated.to_string()));
    }
    object
}

pub(super) fn field_value(field: ContactField, value: &Value) -> Result<Value, JmapError> {
    Ok(match field {
        ContactField::Name => name_value(&decode(value)?),
        ContactField::Nicknames => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<ContactNickname>>,
        >(value)?),
        ContactField::Emails => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<ContactEmail>>,
        >(value)?),
        ContactField::Phones => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<ContactPhone>>,
        >(value)?),
        ContactField::Addresses => address_values(&decode(value)?),
        ContactField::Organizations => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<Organization>>,
        >(value)?),
        ContactField::Titles => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<Title>>,
        >(value)?),
        ContactField::Anniversaries => {
            property_values(&decode::<BTreeMap<PropertyId, ContactProperty<Anniversary>>>(value)?)
        }
        ContactField::Notes => {
            property_values(&decode::<BTreeMap<PropertyId, ContactProperty<ContactNote>>>(value)?)
        }
        ContactField::Urls
        | ContactField::Calendars
        | ContactField::SchedulingAddresses
        | ContactField::CryptoKeys
        | ContactField::Directories => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<ContactResource>>,
        >(value)?),
        ContactField::OnlineServices => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<ContactOnlineService>>,
        >(value)?),
        ContactField::Relations => relation_values(&decode(value)?),
        ContactField::Languages => language_values(&decode(value)?),
        ContactField::PersonalInfo => property_values(&decode::<
            BTreeMap<PropertyId, ContactProperty<PersonalInfo>>,
        >(value)?),
        ContactField::Keywords => bool_set(&decode(value)?),
        ContactField::Kind | ContactField::TimeZone => {
            return Err(JmapError::protocol(format!(
                "unsupported contact field {field:?}"
            )));
        }
    })
}

/// Writes a group Card's `members`.
///
/// RFC 9553 §2.1.7 types this as `String[Boolean]`: the key is the member Card's
/// **`uid`** and the value MUST be `true`. It is therefore not a property-object map
/// — [`insert_property`] would emit the member as a nested object under a synthesized
/// property id, which no server would read as a membership. Per-member
/// contexts/pref/label that the neutral model can carry (a vCard `MEMBER` parameter)
/// have no JSContact representation and are dropped here by design.
fn insert_members(
    object: &mut Map<String, Value>,
    members: &BTreeMap<PropertyId, ContactProperty<ContactMember>>,
) {
    if members.is_empty() {
        return;
    }
    object.insert(
        "members".into(),
        Value::Object(
            members
                .values()
                .map(|member| (member.value.uid.clone(), Value::Bool(true)))
                .collect(),
        ),
    );
}

fn insert_property<T: Serialize>(
    object: &mut Map<String, Value>,
    name: &str,
    values: &BTreeMap<PropertyId, ContactProperty<T>>,
) {
    if !values.is_empty() {
        object.insert(name.into(), property_values(values));
    }
}

fn property_values<T: Serialize>(values: &BTreeMap<PropertyId, ContactProperty<T>>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(id, property)| (id.as_str().to_owned(), property_value(property)))
            .collect(),
    )
}

fn property_value<T: Serialize>(property: &ContactProperty<T>) -> Value {
    let mut value = clean(serde_json::to_value(&property.value).unwrap_or(Value::Null));
    let object = value
        .as_object_mut()
        .expect("contact field values are objects");
    if !property.contexts.is_empty() {
        object.insert("contexts".into(), bool_set(&property.contexts));
    }
    if let Some(preference) = property.preference {
        object.insert("pref".into(), json!(preference));
    }
    if let Some(label) = &property.label {
        object.insert("label".into(), json!(label));
    }
    for (key, value) in property.extensions.iter() {
        object.insert(key.clone(), value.clone());
    }
    value
}

fn address_values(values: &BTreeMap<PropertyId, ContactProperty<ContactAddress>>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|(id, property)| {
                let mut value = property_value(property);
                let object = value.as_object_mut().expect("address is an object");
                object.insert(
                    "components".into(),
                    Value::Array(
                        property
                            .value
                            .components
                            .iter()
                            .flat_map(|(kind, values)| {
                                values
                                    .iter()
                                    .map(move |value| json!({"kind": kind, "value": value}))
                            })
                            .collect(),
                    ),
                );
                (id.as_str().to_owned(), value)
            })
            .collect(),
    )
}

fn relation_values(values: &BTreeMap<PropertyId, ContactProperty<ContactRelation>>) -> Value {
    Value::Object(
        values
            .values()
            .filter_map(|property| {
                let id = property
                    .value
                    .uid
                    .as_ref()
                    .or(property.value.uri.as_ref())?;
                Some((
                    id.clone(),
                    json!({"relation": bool_set(&property.value.relation)}),
                ))
            })
            .collect(),
    )
}

fn language_values(values: &BTreeMap<PropertyId, ContactProperty<ContactLanguage>>) -> Value {
    Value::Object(
        values
            .values()
            .map(|property| {
                let mut value = Map::new();
                if let Some(preference) = property.preference {
                    value.insert("pref".into(), json!(preference));
                }
                (property.value.language.clone(), Value::Object(value))
            })
            .collect(),
    )
}

fn name_value(name: &ContactName) -> Value {
    let components = name
        .components
        .iter()
        .map(|component| {
            let kind = match &component.kind {
                engine_core::contact::NameComponentKind::Prefix => "title",
                engine_core::contact::NameComponentKind::Given => "given",
                engine_core::contact::NameComponentKind::Middle => "given2",
                engine_core::contact::NameComponentKind::Surname => "surname",
                engine_core::contact::NameComponentKind::Surname2 => "surname2",
                engine_core::contact::NameComponentKind::Suffix => "credential",
                engine_core::contact::NameComponentKind::Other(value) => value,
            };
            json!({"kind": kind, "value": component.value})
        })
        .collect::<Vec<_>>();
    json!({
        "full": name.full,
        "components": components,
        "sortAs": name.sort_as,
        "phoneticSystem": name.phonetic_system
    })
}

fn bool_set(values: &BTreeSet<String>) -> Value {
    Value::Object(
        values
            .iter()
            .map(|value| (value.clone(), Value::Bool(true)))
            .collect(),
    )
}

fn clean(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(key, value)| key != "extensions" && !value.is_null())
                .map(|(key, value)| {
                    let key = match key.as_str() {
                        "sort_as" => "sortAs",
                        "country_code" => "countryCode",
                        "time_zone" => "timeZone",
                        "organization_id" => "organizationId",
                        "media_type" => "mediaType",
                        "fingerprint" => "blobId",
                        other => other,
                    };
                    let value = match (key, value) {
                        ("features", Value::Array(values)) => Value::Object(
                            values
                                .into_iter()
                                .filter_map(|value| value.as_str().map(str::to_owned))
                                .map(|value| (value, Value::Bool(true)))
                                .collect(),
                        ),
                        (_, value) => clean(value),
                    };
                    (key.to_owned(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(clean).collect()),
        other => other,
    }
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, JmapError> {
    serde_json::from_value(value.clone()).map_err(JmapError::from)
}
