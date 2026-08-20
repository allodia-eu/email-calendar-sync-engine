//! Microsoft Graph contact/folder normalization.

use std::collections::BTreeMap;

use engine_core::{
    contact::{
        AddressBook, Anniversary, ContactAddress, ContactCard, ContactEmail, ContactKind,
        ContactName, ContactNote, ContactPhone, ContactProperty, ContactResource,
        ContactSourceClass, NameComponent, NameComponentKind, Organization, OrganizationUnit,
        PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawProviderJson,
    version::{ChangeKey, RevisionTokens},
};
use serde_json::Value;

use crate::error::GraphError;

pub(crate) fn folder(value: &Value) -> Result<AddressBook, GraphError> {
    let id = text(value, "id")?;
    let mut book = AddressBook::new(
        AddressBookId::try_from(id).map_err(|error| GraphError::protocol(error.to_string()))?,
        value
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("Contacts"),
        ContactSourceClass::Personal,
    );
    book.is_writable = true;
    book.owner = value
        .get("parentFolderId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    book.raw_provider_json = Some(raw(value)?);
    Ok(book)
}

pub(crate) fn card(
    value: &Value,
    address_book: AddressBookId,
    source_class: ContactSourceClass,
    writable: bool,
) -> Result<ContactCard, GraphError> {
    let id = ContactId::try_from(text(value, "id")?)
        .map_err(|error| GraphError::protocol(error.to_string()))?;
    let mut card = ContactCard::new(id, Memberships::of_one(address_book));
    card.source_class = source_class;
    card.is_writable = writable;
    card.uid = value
        .get("id")
        .and_then(Value::as_str)
        .map(|id| format!("urn:microsoft:graph:{id}"));
    card.kind = if source_class == ContactSourceClass::Directory
        && value
            .get("userType")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("guest"))
    {
        ContactKind::Other("guest".into())
    } else {
        ContactKind::Individual
    };
    card.name = contact_name(value);
    normalize_emails(value, &mut card)?;
    normalize_phones(value, &mut card)?;
    normalize_addresses(value, &mut card)?;
    normalize_organization(value, &mut card)?;
    if let Some(notes) = value
        .get("personalNotes")
        .or_else(|| value.get("notes"))
        .and_then(Value::as_str)
        .filter(|notes| !notes.is_empty())
    {
        card.notes.insert(
            property_id("notes")?,
            ContactProperty::new(ContactNote::new(notes)),
        );
    }
    if let Some(birthday) = value.get("birthday").and_then(Value::as_str) {
        // Graph stores a birthday as an *instant* anchored near local noon, so it reads
        // back as a full timestamp ("1815-12-10T11:59:00Z") even when set from a bare
        // date. `Anniversary.date` is JSContact date text, so keep only the date part —
        // otherwise the time component leaks into a neutral field the CardDAV and Google
        // adapters fill with `YYYY-MM-DD`.
        let date = birthday.split_once('T').map_or(birthday, |(date, _)| date);
        card.anniversaries.insert(
            property_id("birthday")?,
            ContactProperty::new(Anniversary {
                date: date.into(),
                kind: Some("birth".into()),
                place: None,
            }),
        );
    }
    // `categories` is the Graph counterpart of neutral keywords, and `contact_write`
    // already maps `ContactField::Keywords` onto it — reading it back is what keeps the
    // round-trip lossless.
    if let Some(categories) = value.get("categories").and_then(Value::as_array) {
        card.keywords.extend(
            categories
                .iter()
                .filter_map(Value::as_str)
                .filter(|category| !category.is_empty())
                .map(str::to_owned),
        );
    }
    // `businessHomePage` is the only web address a Graph `contact` carries; the
    // resource has no personal-homepage counterpart, so there is nothing to pair it
    // with. (A second entry here used to read `personalNotes` — the notes field — and
    // republished any note starting with `http` as a URL resource.)
    if let Some(uri) = value.get("businessHomePage").and_then(Value::as_str)
        && uri.starts_with("http")
    {
        card.urls.insert(
            property_id("business-homepage")?,
            ContactProperty::new(ContactResource {
                uri: uri.into(),
                kind: Some("work".into()),
                ..ContactResource::default()
            }),
        );
    }
    // Every Graph contact and directory user has a photo *endpoint*, so the card
    // advertises the resource with an empty URI and the fetch path derives the URL
    // from the card id. Whether an image is actually stored there is only knowable by
    // asking, which is precisely the question a host must be able to put: without
    // this entry, "this card has no photo" and "Graph does not tell us about photos"
    // are the same silence, and the other three adapters answer the first one.
    card.media.insert(
        property_id("photo")?,
        ContactProperty::new(ContactResource {
            kind: Some("photo".into()),
            ..ContactResource::default()
        }),
    );
    if let Some(change_key) = value.get("changeKey").and_then(Value::as_str) {
        card.revisions = RevisionTokens {
            change_key: Some(ChangeKey::new(change_key)),
            ..RevisionTokens::none()
        };
    }
    card.raw_provider_json = Some(raw(value)?);
    Ok(card)
}

fn contact_name(value: &Value) -> Option<ContactName> {
    let mut components = Vec::new();
    for (key, kind) in [
        ("title", NameComponentKind::Prefix),
        ("givenName", NameComponentKind::Given),
        ("middleName", NameComponentKind::Middle),
        ("surname", NameComponentKind::Surname),
        ("generation", NameComponentKind::Suffix),
    ] {
        if let Some(text) = value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            components.push(NameComponent::new(kind, text));
        }
    }
    let full = value
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    (full.is_some() || !components.is_empty()).then_some(ContactName {
        full,
        components,
        ..ContactName::default()
    })
}

fn normalize_emails(value: &Value, card: &mut ContactCard) -> Result<(), GraphError> {
    let Some(emails) = value
        .get("emailAddresses")
        .or_else(|| value.get("proxyAddresses"))
        .and_then(Value::as_array)
    else {
        if let Some(email) = value
            .get("mail")
            .or_else(|| value.get("userPrincipalName"))
            .and_then(Value::as_str)
        {
            card.emails.insert(
                property_id("primary")?,
                ContactProperty::new(ContactEmail::new(email)),
            );
        }
        return Ok(());
    };
    for (index, email) in emails.iter().enumerate() {
        let address = email
            .get("address")
            .and_then(Value::as_str)
            .or_else(|| {
                email
                    .as_str()
                    .and_then(|text| text.split_once(':').map(|(_, v)| v))
            })
            .unwrap_or_default();
        if address.is_empty() {
            continue;
        }
        let mut property = ContactProperty::new(ContactEmail::new(address));
        property.label = email.get("name").and_then(Value::as_str).map(str::to_owned);
        card.emails
            .insert(property_id(&format!("email-{index}"))?, property);
    }
    Ok(())
}

fn normalize_phones(value: &Value, card: &mut ContactCard) -> Result<(), GraphError> {
    let mut phones = Vec::new();
    for (key, context) in [("businessPhones", "work"), ("homePhones", "private")] {
        for phone in value
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            phones.push((phone, context, false));
        }
    }
    if let Some(phone) = value
        .get("mobilePhone")
        .or_else(|| value.get("mobile"))
        .and_then(Value::as_str)
    {
        phones.push((phone, "private", true));
    }
    for (index, (number, context, mobile)) in phones.into_iter().enumerate() {
        let mut phone = ContactPhone {
            number: number.into(),
            ..ContactPhone::default()
        };
        if mobile {
            phone.features.insert("mobile".into());
        }
        let mut property = ContactProperty::new(phone);
        property.contexts.insert(context.into());
        card.phones
            .insert(property_id(&format!("phone-{index}"))?, property);
    }
    Ok(())
}

fn normalize_addresses(value: &Value, card: &mut ContactCard) -> Result<(), GraphError> {
    for (key, id, context) in [
        ("businessAddress", "business", "work"),
        ("homeAddress", "home", "private"),
        ("otherAddress", "other", "other"),
    ] {
        let Some(address) = value.get(key).filter(|address| address.is_object()) else {
            continue;
        };
        let mut components = BTreeMap::new();
        for (field, normalized) in [
            ("street", "street"),
            ("city", "locality"),
            ("state", "region"),
            ("postalCode", "postcode"),
            ("countryOrRegion", "country"),
        ] {
            if let Some(text) = address
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                components.insert(normalized.into(), vec![text.into()]);
            }
        }
        if components.is_empty() {
            continue;
        }
        let mut property = ContactProperty::new(ContactAddress {
            components,
            country_code: address
                .get("countryOrRegion")
                .and_then(Value::as_str)
                .filter(|country| country.len() == 2)
                .map(str::to_owned),
            ..ContactAddress::default()
        });
        property.contexts.insert(context.into());
        card.addresses.insert(property_id(id)?, property);
    }
    Ok(())
}

fn normalize_organization(value: &Value, card: &mut ContactCard) -> Result<(), GraphError> {
    if let Some(company) = value
        .get("companyName")
        .and_then(Value::as_str)
        .filter(|company| !company.is_empty())
    {
        let units = value
            .get("department")
            .and_then(Value::as_str)
            .filter(|unit| !unit.is_empty())
            .map(|unit| {
                vec![OrganizationUnit {
                    name: unit.into(),
                    ..OrganizationUnit::default()
                }]
            })
            .unwrap_or_default();
        card.organizations.insert(
            property_id("organization")?,
            ContactProperty::new(Organization {
                name: company.into(),
                units,
                ..Organization::default()
            }),
        );
    }
    if let Some(title) = value
        .get("jobTitle")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
    {
        card.titles.insert(
            property_id("job-title")?,
            ContactProperty::new(Title {
                name: title.into(),
                kind: Some("title".into()),
                organization_id: Some(property_id("organization")?),
            }),
        );
    }
    Ok(())
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, GraphError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| GraphError::protocol(format!("contact missing {key}")))
}

fn property_id(value: &str) -> Result<PropertyId, GraphError> {
    PropertyId::new(value).map_err(|error| GraphError::protocol(error.to_string()))
}

fn raw(value: &Value) -> Result<RawProviderJson, GraphError> {
    Ok(RawProviderJson::new(serde_json::to_string(value)?))
}

#[cfg(test)]
#[path = "contact_normalize_tests.rs"]
mod tests;
