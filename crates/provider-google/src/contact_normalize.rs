//! Google People JSON to provider-neutral contact cards.

use std::collections::BTreeSet;

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactCard, ContactEmail, ContactName, ContactNickname,
        ContactNote, ContactPhone, ContactProperty, ContactRelation, ContactResource,
        ContactSourceClass, NameComponent, NameComponentKind, Organization, OrganizationUnit,
        PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawProviderJson,
    version::{ETag, RevisionTokens},
};
use serde_json::Value;

use crate::error::GoogleError;

pub(crate) fn person(
    value: &Value,
    address_book: AddressBookId,
    source_class: ContactSourceClass,
    writable: bool,
) -> Result<ContactCard, GoogleError> {
    let resource = required_text(value, "resourceName")?;
    let id =
        ContactId::try_from(resource).map_err(|error| GoogleError::protocol(error.to_string()))?;
    let mut card = ContactCard::new(id, Memberships::of_one(address_book));
    card.uid = Some(resource.to_owned());
    card.source_class = source_class;
    card.is_writable = writable;
    card.name = value
        .get("names")
        .and_then(Value::as_array)
        .and_then(|names| names.first())
        .map(name);
    card.nicknames = values(value, "nicknames")
        .enumerate()
        .filter_map(|(index, entry)| {
            let nickname = entry.get("value")?.as_str()?;
            Some(property(
                entry,
                ContactNickname::new(nickname),
                "nickname",
                index,
            ))
        })
        .collect();
    card.emails = values(value, "emailAddresses")
        .enumerate()
        .filter_map(|(index, entry)| {
            let address = entry.get("value")?.as_str()?;
            Some(property(entry, ContactEmail::new(address), "email", index))
        })
        .collect();
    card.phones = values(value, "phoneNumbers")
        .enumerate()
        .filter_map(|(index, entry)| {
            let number = entry.get("value")?.as_str()?;
            let mut phone = ContactPhone {
                number: number.to_owned(),
                ..ContactPhone::default()
            };
            if entry.get("type").and_then(Value::as_str) == Some("mobile") {
                phone.features.insert("mobile".into());
            }
            Some(property(entry, phone, "phone", index))
        })
        .collect();
    card.addresses = values(value, "addresses")
        .enumerate()
        .map(|(index, entry)| {
            let mut address = ContactAddress {
                full: entry
                    .get("formattedValue")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                country_code: entry
                    .get("countryCode")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                ..ContactAddress::default()
            };
            for (provider, normalized) in [
                ("streetAddress", "street"),
                ("city", "locality"),
                ("region", "region"),
                ("postalCode", "postcode"),
                ("country", "country"),
            ] {
                if let Some(text) = entry.get(provider).and_then(Value::as_str) {
                    address
                        .components
                        .insert(normalized.into(), vec![text.to_owned()]);
                }
            }
            property(entry, address, "address", index)
        })
        .collect();
    normalize_organizations(value, &mut card);
    normalize_birthdays(value, &mut card);
    card.notes = values(value, "biographies")
        .enumerate()
        .filter_map(|(index, entry)| {
            let note = entry.get("value")?.as_str()?;
            Some(property(entry, ContactNote::new(note), "note", index))
        })
        .collect();
    card.urls = values(value, "urls")
        .enumerate()
        .filter_map(|(index, entry)| {
            let uri = entry.get("value")?.as_str()?;
            Some(property(
                entry,
                ContactResource {
                    uri: uri.to_owned(),
                    ..ContactResource::default()
                },
                "url",
                index,
            ))
        })
        .collect();
    card.relations = values(value, "relations")
        .enumerate()
        .filter_map(|(index, entry)| {
            let person = entry.get("person")?.as_str()?;
            let relation = entry
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .into_iter()
                .collect();
            Some(property(
                entry,
                ContactRelation {
                    relation,
                    uid: Some(person.to_owned()),
                    uri: None,
                },
                "relation",
                index,
            ))
        })
        .collect();
    card.keywords = values(value, "userDefined")
        .filter_map(|entry| {
            let key = entry.get("key").and_then(Value::as_str)?;
            let value = entry.get("value").and_then(Value::as_str)?;
            Some(if key == "category" {
                value.to_owned()
            } else {
                format!("{key}:{value}")
            })
        })
        .collect();
    card.media = values(value, "photos")
        .filter(|entry| entry.get("default").and_then(Value::as_bool) != Some(true))
        .enumerate()
        .filter_map(|(index, entry)| {
            let uri = entry.get("url")?.as_str()?;
            Some(property(
                entry,
                ContactResource {
                    uri: uri.to_owned(),
                    kind: Some("photo".into()),
                    media_type: None,
                    title: None,
                    fingerprint: value.get("etag").and_then(Value::as_str).map(str::to_owned),
                },
                "photo",
                index,
            ))
        })
        .collect();
    if let Some(etag) = value.get("etag").and_then(Value::as_str) {
        card.revisions = RevisionTokens::from_etag(ETag::new(etag));
    }
    card.raw_provider_json = Some(RawProviderJson::new(value.to_string()));
    Ok(card)
}

pub(crate) fn source_books() -> Vec<engine_core::contact::AddressBook> {
    use engine_core::contact::AddressBook;
    [
        (
            "google-connections",
            "Contacts",
            ContactSourceClass::Personal,
            true,
        ),
        (
            "google-other-contacts",
            "Other contacts",
            ContactSourceClass::Suggested,
            false,
        ),
        (
            "google-directory",
            "Directory",
            ContactSourceClass::Directory,
            false,
        ),
        (
            "google-contact-groups",
            "Contact groups",
            ContactSourceClass::Personal,
            false,
        ),
    ]
    .into_iter()
    .map(|(id, name, class, writable)| {
        let mut book =
            AddressBook::new(AddressBookId::try_from(id).expect("static id"), name, class);
        book.is_writable = writable;
        book
    })
    .collect()
}

pub(crate) fn group_card(value: &Value, book: AddressBookId) -> Result<ContactCard, GoogleError> {
    let resource = value
        .get("resourceName")
        .and_then(Value::as_str)
        .ok_or_else(|| GoogleError::protocol("contact group missing resourceName"))?;
    let mut card = ContactCard::new(
        ContactId::try_from(resource).map_err(|error| GoogleError::protocol(error.to_string()))?,
        Memberships::of_one(book),
    );
    card.kind = engine_core::contact::ContactKind::Group;
    card.name = Some(ContactName {
        full: value.get("name").and_then(Value::as_str).map(str::to_owned),
        ..ContactName::default()
    });
    card.source_class = ContactSourceClass::Personal;
    card.raw_provider_json = Some(RawProviderJson::new(value.to_string()));
    Ok(card)
}

pub(crate) fn deleted(value: &Value) -> bool {
    value
        .get("metadata")
        .and_then(|metadata| metadata.get("deleted"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn name(value: &Value) -> ContactName {
    let mut name = ContactName {
        full: value
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..ContactName::default()
    };
    for (field, kind) in [
        ("honorificPrefix", NameComponentKind::Prefix),
        ("givenName", NameComponentKind::Given),
        ("middleName", NameComponentKind::Middle),
        ("familyName", NameComponentKind::Surname),
        ("honorificSuffix", NameComponentKind::Suffix),
    ] {
        if let Some(text) = value.get(field).and_then(Value::as_str) {
            name.components.push(NameComponent::new(kind, text));
        }
    }
    name
}

fn normalize_organizations(value: &Value, card: &mut ContactCard) {
    for (index, entry) in values(value, "organizations").enumerate() {
        let organization_id = property_id("organization", index);
        let organization = Organization {
            name: entry
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            units: entry
                .get("department")
                .and_then(Value::as_str)
                .map(|name| {
                    vec![OrganizationUnit {
                        name: name.to_owned(),
                        ..OrganizationUnit::default()
                    }]
                })
                .unwrap_or_default(),
            ..Organization::default()
        };
        card.organizations.insert(
            organization_id.clone(),
            contact_property(entry, organization),
        );
        if let Some(title) = entry.get("title").and_then(Value::as_str) {
            card.titles.insert(
                property_id("title", index),
                contact_property(
                    entry,
                    Title {
                        name: title.to_owned(),
                        kind: Some("title".into()),
                        organization_id: Some(organization_id),
                    },
                ),
            );
        }
    }
}

fn normalize_birthdays(value: &Value, card: &mut ContactCard) {
    for (index, entry) in values(value, "birthdays").enumerate() {
        let Some(date) = entry.get("date").and_then(Value::as_object) else {
            continue;
        };
        let text = format!(
            "{:04}-{:02}-{:02}",
            date.get("year").and_then(Value::as_u64).unwrap_or(0),
            date.get("month").and_then(Value::as_u64).unwrap_or(0),
            date.get("day").and_then(Value::as_u64).unwrap_or(0)
        );
        card.anniversaries.insert(
            property_id("birthday", index),
            contact_property(
                entry,
                Anniversary {
                    date: text,
                    kind: Some("birth".into()),
                    place: None,
                },
            ),
        );
    }
}

fn property<T>(
    entry: &Value,
    value: T,
    prefix: &str,
    index: usize,
) -> (PropertyId, ContactProperty<T>) {
    (property_id(prefix, index), contact_property(entry, value))
}

/// Keys one property of a card by field and position.
///
/// The People API's `metadata.source.id` looks like a per-property key but is not:
/// it identifies the source *record* (RFC-equivalent: the whole contact), so every
/// email, phone, address, and organization of one person carries the same value.
/// Using it collapsed each multi-valued field into a single map entry and silently
/// dropped everything but the last. Position within the field is what actually
/// distinguishes Google's entries, and it matches the fallback the CardDAV and Graph
/// adapters already use.
fn property_id(prefix: &str, index: usize) -> PropertyId {
    PropertyId::new(format!("{prefix}-{index}")).expect("prefixed index is never empty")
}

fn contact_property<T>(entry: &Value, value: T) -> ContactProperty<T> {
    let mut property = ContactProperty::new(value);
    if let Some(context) = entry.get("type").and_then(Value::as_str) {
        property.contexts = BTreeSet::from([match context {
            "home" => "private".to_owned(),
            other => other.to_owned(),
        }]);
    }
    if entry
        .get("metadata")
        .and_then(|metadata| metadata.get("primary"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        property.preference = Some(1);
    }
    property
}

fn values<'a>(value: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    value
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str, GoogleError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| GoogleError::protocol(format!("Google person missing {field}")))
}

#[cfg(test)]
#[path = "contact_normalize_tests.rs"]
mod tests;
