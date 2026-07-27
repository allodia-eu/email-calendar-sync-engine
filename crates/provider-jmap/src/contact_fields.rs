//! Normalization of JSContact property maps beyond identity and membership.

use std::collections::{BTreeMap, BTreeSet};

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactCard, ContactLanguage, ContactNickname, ContactNote,
        ContactOnlineService, ContactPhone, ContactProperty, ContactRelation, ContactResource,
        Organization, OrganizationUnit, PersonalInfo, PropertyId, Title,
    },
    time::UtcDateTime,
};
use serde_json::Value;

use crate::error::JmapError;

pub(super) fn apply(card: &mut ContactCard, value: &Value) -> Result<(), JmapError> {
    card.nicknames = property_map(value.get("nicknames"), |_, entry| {
        Some(ContactNickname::new(entry.get("name")?.as_str()?))
    })?;
    card.phones = property_map(value.get("phones"), |_, entry| {
        Some(ContactPhone {
            number: entry.get("number")?.as_str()?.to_owned(),
            features: true_keys(entry.get("features")),
        })
    })?;
    card.addresses = property_map(value.get("addresses"), |_, entry| {
        Some(ContactAddress {
            full: text(entry, "full"),
            components: address_components(entry),
            country_code: text(entry, "countryCode"),
            coordinates: text(entry, "coordinates"),
            time_zone: text(entry, "timeZone"),
            ..ContactAddress::default()
        })
    })?;
    card.organizations = property_map(value.get("organizations"), |_, entry| {
        Some(Organization {
            name: entry.get("name")?.as_str()?.to_owned(),
            units: entry
                .get("units")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|unit| {
                    Some(OrganizationUnit {
                        name: unit.get("name")?.as_str()?.to_owned(),
                        sort_as: text(unit, "sortAs"),
                        ..OrganizationUnit::default()
                    })
                })
                .collect(),
            ..Organization::default()
        })
    })?;
    card.titles = property_map(value.get("titles"), |_, entry| {
        Some(Title {
            name: entry.get("name")?.as_str()?.to_owned(),
            kind: text(entry, "kind"),
            organization_id: entry
                .get("organizationId")
                .and_then(Value::as_str)
                .and_then(|id| PropertyId::new(id).ok()),
        })
    })?;
    card.anniversaries = property_map(value.get("anniversaries"), |_, entry| {
        Some(Anniversary {
            date: entry.get("date")?.as_str()?.to_owned(),
            kind: text(entry, "kind"),
            place: entry
                .get("place")
                .and_then(|place| {
                    place
                        .get("full")
                        .and_then(Value::as_str)
                        .or_else(|| place.as_str())
                })
                .map(str::to_owned),
        })
    })?;
    card.notes = property_map(value.get("notes"), |_, entry| {
        Some(ContactNote::new(entry.get("note")?.as_str()?))
    })?;
    card.urls = resource_map(value.get("links"))?;
    card.media = resource_map(value.get("media"))?;
    card.online_services = property_map(value.get("onlineServices"), |_, entry| {
        Some(ContactOnlineService {
            service: text(entry, "service"),
            user: text(entry, "user"),
            uri: text(entry, "uri"),
        })
    })?;
    card.relations = property_map(value.get("relatedTo"), |id, entry| {
        Some(ContactRelation {
            relation: true_keys(entry.get("relation")),
            uid: Some(id.to_owned()),
            uri: None,
        })
    })?;
    card.languages = property_map(value.get("preferredLanguages"), |id, _| {
        Some(ContactLanguage::new(id))
    })?;
    card.personal_info = property_map(value.get("personalInfo"), |_, entry| {
        Some(PersonalInfo {
            kind: entry.get("kind")?.as_str()?.to_owned(),
            value: entry.get("value")?.as_str()?.to_owned(),
        })
    })?;
    card.calendars = resource_map(value.get("calendars"))?;
    card.scheduling_addresses = resource_map(value.get("schedulingAddresses"))?;
    card.crypto_keys = resource_map(value.get("cryptoKeys"))?;
    card.directories = resource_map(value.get("directories"))?;
    card.keywords = true_keys(value.get("keywords"));
    card.created = timestamp(value, "created")?;
    card.updated = timestamp(value, "updated")?;
    preserve_extensions(card, value);
    Ok(())
}

fn property_map<T>(
    value: Option<&Value>,
    normalize: impl Fn(&str, &Value) -> Option<T>,
) -> Result<BTreeMap<PropertyId, ContactProperty<T>>, JmapError> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(|(id, entry)| normalize(id, entry).map(|normalized| (id, entry, normalized)))
        .map(|(id, entry, normalized)| {
            let mut property = ContactProperty::new(normalized);
            property.contexts = true_keys(entry.get("contexts"));
            property.preference = entry
                .get("pref")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok());
            property.label = text(entry, "label");
            Ok((
                PropertyId::new(id).map_err(|error| JmapError::protocol(error.to_string()))?,
                property,
            ))
        })
        .collect()
}

fn resource_map(
    value: Option<&Value>,
) -> Result<BTreeMap<PropertyId, ContactProperty<ContactResource>>, JmapError> {
    property_map(value, |_, entry| {
        Some(ContactResource {
            uri: entry.get("uri")?.as_str()?.to_owned(),
            kind: text(entry, "kind"),
            media_type: text(entry, "mediaType").or_else(|| text(entry, "type")),
            title: text(entry, "title").or_else(|| text(entry, "name")),
            fingerprint: text(entry, "blobId"),
        })
    })
}

fn address_components(value: &Value) -> BTreeMap<String, Vec<String>> {
    let mut components = BTreeMap::new();
    for component in value
        .get("components")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let (Some(kind), Some(value)) = (
            component.get("kind").and_then(Value::as_str),
            component.get("value").and_then(Value::as_str),
        ) else {
            continue;
        };
        components
            .entry(kind.to_owned())
            .or_insert_with(Vec::new)
            .push(value.to_owned());
    }
    components
}

fn true_keys(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(_, present)| present.as_bool() == Some(true))
        .map(|(key, _)| key.clone())
        .collect()
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn timestamp(value: &Value, key: &str) -> Result<Option<UtcDateTime>, JmapError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(UtcDateTime::parse_rfc3339)
        .transpose()
        .map_err(|error| JmapError::protocol(format!("invalid JSContact {key}: {error}")))
}

fn preserve_extensions(card: &mut ContactCard, value: &Value) {
    const KNOWN: &[&str] = &[
        "@type",
        "id",
        "version",
        "uid",
        "kind",
        "name",
        "nicknames",
        "emails",
        "phones",
        "addresses",
        "organizations",
        "titles",
        "anniversaries",
        "notes",
        "links",
        "media",
        "onlineServices",
        "relatedTo",
        "preferredLanguages",
        "members",
        "personalInfo",
        "calendars",
        "schedulingAddresses",
        "cryptoKeys",
        "directories",
        "keywords",
        "created",
        "updated",
        "addressBookIds",
        "isReadOnly",
    ];
    for (key, value) in value.as_object().into_iter().flat_map(|map| map.iter()) {
        if !KNOWN.contains(&key.as_str()) {
            card.extended.set(format!("jscontact/{key}"), value.clone());
        }
    }
}

#[cfg(test)]
#[path = "contact_fields_tests.rs"]
mod tests;
