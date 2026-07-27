//! Microsoft Graph contact create/patch request bodies.

use std::collections::BTreeMap;

use engine_core::contact::{
    ContactAddress, ContactCard, ContactEmail, ContactField, ContactName, ContactNote,
    ContactPatch, ContactPhone, ContactProperty, FieldPatch, Organization, PropertyId, Title,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::error::GraphError;

pub(crate) fn create_body(card: &ContactCard) -> Result<Vec<u8>, GraphError> {
    Ok(serde_json::to_vec(&card_fields(card))?)
}

pub(crate) fn patch_body(patch: &ContactPatch) -> Result<Vec<u8>, GraphError> {
    if matches!(
        patch.kind,
        Some(FieldPatch::Set(ref kind)) if *kind != engine_core::contact::ContactKind::Individual
    ) || matches!(patch.kind, Some(FieldPatch::Clear))
    {
        return Err(GraphError::protocol(
            "Graph personal contacts support only individual cards",
        ));
    }
    let mut body = Map::new();
    for (field, patch) in &patch.fields {
        if matches!(patch, FieldPatch::Clear) {
            clear_field(&mut body, *field);
            continue;
        }
        let FieldPatch::Set(value) = patch else {
            continue;
        };
        match field {
            ContactField::Name => {
                let name: ContactName = decode(value)?;
                insert_name(&mut body, &name);
            }
            ContactField::Emails => {
                let values: BTreeMap<PropertyId, ContactProperty<ContactEmail>> = decode(value)?;
                body.insert("emailAddresses".into(), emails(&values));
            }
            ContactField::Phones => {
                let values: BTreeMap<PropertyId, ContactProperty<ContactPhone>> = decode(value)?;
                insert_phones(&mut body, &values);
            }
            ContactField::Addresses => {
                let values: BTreeMap<PropertyId, ContactProperty<ContactAddress>> = decode(value)?;
                insert_addresses(&mut body, &values);
            }
            ContactField::Organizations => {
                let values: BTreeMap<PropertyId, ContactProperty<Organization>> = decode(value)?;
                insert_organizations(&mut body, &values);
            }
            ContactField::Titles => {
                let values: BTreeMap<PropertyId, ContactProperty<Title>> = decode(value)?;
                if let Some(title) = values.values().next() {
                    body.insert("jobTitle".into(), json!(title.value.name));
                }
            }
            ContactField::Notes => {
                let values: BTreeMap<PropertyId, ContactProperty<ContactNote>> = decode(value)?;
                body.insert(
                    "personalNotes".into(),
                    json!(
                        values
                            .values()
                            .map(|note| note.value.note.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                );
            }
            ContactField::Keywords => {
                body.insert("categories".into(), value.clone());
            }
            _ => {
                return Err(GraphError::protocol(format!(
                    "unsupported Graph contact patch field {field:?}"
                )));
            }
        }
    }
    Ok(serde_json::to_vec(&Value::Object(body))?)
}

fn card_fields(card: &ContactCard) -> Value {
    let mut body = Map::new();
    if let Some(name) = &card.name {
        insert_name(&mut body, name);
    }
    body.insert("emailAddresses".into(), emails(&card.emails));
    insert_phones(&mut body, &card.phones);
    insert_addresses(&mut body, &card.addresses);
    insert_organizations(&mut body, &card.organizations);
    if let Some(title) = card.titles.values().next() {
        body.insert("jobTitle".into(), json!(title.value.name));
    }
    if !card.notes.is_empty() {
        body.insert(
            "personalNotes".into(),
            json!(
                card.notes
                    .values()
                    .map(|note| note.value.note.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        );
    }
    if !card.keywords.is_empty() {
        body.insert(
            "categories".into(),
            json!(card.keywords.iter().collect::<Vec<_>>()),
        );
    }
    Value::Object(body)
}

fn insert_name(body: &mut Map<String, Value>, name: &ContactName) {
    if let Some(full) = &name.full {
        body.insert("displayName".into(), json!(full));
    }
    for component in &name.components {
        let key = match component.kind {
            engine_core::contact::NameComponentKind::Prefix => "title",
            engine_core::contact::NameComponentKind::Given => "givenName",
            engine_core::contact::NameComponentKind::Middle => "middleName",
            engine_core::contact::NameComponentKind::Surname => "surname",
            engine_core::contact::NameComponentKind::Suffix => "generation",
            _ => continue,
        };
        body.insert(key.into(), json!(component.value));
    }
}

fn emails(values: &BTreeMap<PropertyId, ContactProperty<ContactEmail>>) -> Value {
    Value::Array(
        values
            .iter()
            .map(|(id, email)| {
                json!({
                    "name": email.label.as_deref().unwrap_or(id.as_str()),
                    "address": email.value.address
                })
            })
            .collect(),
    )
}

fn insert_phones(
    body: &mut Map<String, Value>,
    values: &BTreeMap<PropertyId, ContactProperty<ContactPhone>>,
) {
    let mut business = Vec::new();
    let mut home = Vec::new();
    let mut mobile = None;
    for phone in values.values() {
        if phone.value.features.contains("mobile") {
            mobile = Some(phone.value.number.clone());
        } else if phone.contexts.contains("work") {
            business.push(phone.value.number.clone());
        } else {
            home.push(phone.value.number.clone());
        }
    }
    body.insert("businessPhones".into(), json!(business));
    body.insert("homePhones".into(), json!(home));
    body.insert("mobilePhone".into(), json!(mobile));
}

fn insert_addresses(
    body: &mut Map<String, Value>,
    values: &BTreeMap<PropertyId, ContactProperty<ContactAddress>>,
) {
    for address in values.values() {
        let key = if address.contexts.contains("work") {
            "businessAddress"
        } else if address.contexts.contains("private") {
            "homeAddress"
        } else {
            "otherAddress"
        };
        body.insert(key.into(), graph_address(&address.value));
    }
}

fn graph_address(address: &ContactAddress) -> Value {
    let component = |name: &str| {
        address
            .components
            .get(name)
            .and_then(|values| values.first())
            .cloned()
    };
    json!({
        "street": component("street"),
        "city": component("locality"),
        "state": component("region"),
        "postalCode": component("postcode"),
        "countryOrRegion": component("country")
            .or_else(|| address.country_code.clone())
    })
}

fn insert_organizations(
    body: &mut Map<String, Value>,
    values: &BTreeMap<PropertyId, ContactProperty<Organization>>,
) {
    if let Some(organization) = values.values().next() {
        body.insert("companyName".into(), json!(organization.value.name));
        if let Some(unit) = organization.value.units.first() {
            body.insert("department".into(), json!(unit.name));
        }
    }
}

fn clear_field(body: &mut Map<String, Value>, field: ContactField) {
    for key in match field {
        ContactField::Name => &[
            "displayName",
            "title",
            "givenName",
            "middleName",
            "surname",
            "generation",
        ][..],
        ContactField::Emails => &["emailAddresses"],
        ContactField::Phones => &["businessPhones", "homePhones", "mobilePhone"],
        ContactField::Addresses => &["businessAddress", "homeAddress", "otherAddress"],
        ContactField::Organizations => &["companyName", "department"],
        ContactField::Titles => &["jobTitle"],
        ContactField::Notes => &["personalNotes"],
        ContactField::Keywords => &["categories"],
        _ => &[],
    } {
        body.insert((*key).into(), Value::Null);
    }
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, GraphError> {
    serde_json::from_value(value.clone()).map_err(GraphError::from)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use engine_core::{
        contact::{
            ContactKind, ContactNote, FieldPatch, NameComponent, NameComponentKind,
            OrganizationUnit,
        },
        ids::{AddressBookId, ContactId},
        membership::Memberships,
    };

    use super::*;

    fn id(value: &str) -> PropertyId {
        PropertyId::new(value).unwrap()
    }

    fn comprehensive_card() -> ContactCard {
        let mut card = ContactCard::new(
            ContactId::try_from("contact").unwrap(),
            Memberships::of_one(AddressBookId::try_from("book").unwrap()),
        );
        card.name = Some(ContactName {
            full: Some("Dr Ada M Lovelace PhD".into()),
            components: vec![
                NameComponent::new(NameComponentKind::Prefix, "Dr"),
                NameComponent::new(NameComponentKind::Given, "Ada"),
                NameComponent::new(NameComponentKind::Middle, "M"),
                NameComponent::new(NameComponentKind::Surname, "Lovelace"),
                NameComponent::new(NameComponentKind::Suffix, "PhD"),
                NameComponent::new(NameComponentKind::Other("ignored".into()), "Ignored"),
            ],
            ..ContactName::default()
        });
        card.emails.insert(
            id("work"),
            ContactProperty {
                label: Some("Work".into()),
                ..ContactProperty::new(ContactEmail::new("ada@example.test"))
            },
        );
        for (id_value, number, contexts, mobile) in [
            ("work", "+1-work", ["work"].as_slice(), false),
            ("home", "+1-home", ["private"].as_slice(), false),
            ("mobile", "+1-mobile", [].as_slice(), true),
        ] {
            let mut phone = ContactPhone {
                number: number.into(),
                ..ContactPhone::default()
            };
            if mobile {
                phone.features.insert("mobile".into());
            }
            let mut property = ContactProperty::new(phone);
            property
                .contexts
                .extend(contexts.iter().map(|value| (*value).to_owned()));
            card.phones.insert(id(id_value), property);
        }
        for (id_value, context) in [
            ("business", "work"),
            ("home-address", "private"),
            ("other", "other"),
        ] {
            card.addresses.insert(
                id(id_value),
                ContactProperty {
                    contexts: BTreeSet::from([context.into()]),
                    ..ContactProperty::new(ContactAddress {
                        components: BTreeMap::from([
                            ("street".into(), vec!["1 Main St".into()]),
                            ("locality".into(), vec!["London".into()]),
                            ("region".into(), vec!["London".into()]),
                            ("postcode".into(), vec!["N1".into()]),
                        ]),
                        country_code: Some("GB".into()),
                        ..ContactAddress::default()
                    })
                },
            );
        }
        card.organizations.insert(
            id("org"),
            ContactProperty::new(Organization {
                name: "Analytical Engines".into(),
                units: vec![OrganizationUnit {
                    name: "Research".into(),
                    ..OrganizationUnit::default()
                }],
                ..Organization::default()
            }),
        );
        card.titles.insert(
            id("title"),
            ContactProperty::new(Title {
                name: "Programmer".into(),
                ..Title::default()
            }),
        );
        card.notes
            .insert(id("note"), ContactProperty::new(ContactNote::new("first")));
        card.notes.insert(
            id("note-2"),
            ContactProperty::new(ContactNote::new("second")),
        );
        card.keywords.extend(["friend".into(), "work".into()]);
        card
    }

    #[test]
    fn comprehensive_create_body_maps_every_supported_graph_field() {
        let body: Value =
            serde_json::from_slice(&create_body(&comprehensive_card()).unwrap()).unwrap();
        assert_eq!(body["displayName"], json!("Dr Ada M Lovelace PhD"));
        assert_eq!(body["givenName"], json!("Ada"));
        assert_eq!(body["emailAddresses"][0]["name"], json!("Work"));
        assert_eq!(body["businessPhones"], json!(["+1-work"]));
        assert_eq!(body["homePhones"], json!(["+1-home"]));
        assert_eq!(body["mobilePhone"], json!("+1-mobile"));
        assert_eq!(body["businessAddress"]["countryOrRegion"], json!("GB"));
        assert_eq!(body["companyName"], json!("Analytical Engines"));
        assert_eq!(body["department"], json!("Research"));
        assert_eq!(body["jobTitle"], json!("Programmer"));
        assert_eq!(body["personalNotes"], json!("first\nsecond"));
        assert_eq!(body["categories"], json!(["friend", "work"]));
    }

    #[test]
    fn patch_body_sets_clears_and_rejects_unsupported_or_malformed_fields() {
        let card = comprehensive_card();
        let mut patch = ContactPatch {
            kind: Some(FieldPatch::Set(ContactKind::Individual)),
            ..ContactPatch::default()
        };
        patch.fields.insert(
            ContactField::Name,
            FieldPatch::Set(serde_json::to_value(card.name.as_ref().unwrap()).unwrap()),
        );
        for (field, value) in [
            (
                ContactField::Emails,
                serde_json::to_value(&card.emails).unwrap(),
            ),
            (
                ContactField::Phones,
                serde_json::to_value(&card.phones).unwrap(),
            ),
            (
                ContactField::Addresses,
                serde_json::to_value(&card.addresses).unwrap(),
            ),
            (
                ContactField::Organizations,
                serde_json::to_value(&card.organizations).unwrap(),
            ),
            (
                ContactField::Titles,
                serde_json::to_value(&card.titles).unwrap(),
            ),
            (
                ContactField::Notes,
                serde_json::to_value(&card.notes).unwrap(),
            ),
            (
                ContactField::Keywords,
                serde_json::to_value(&card.keywords).unwrap(),
            ),
        ] {
            patch.fields.insert(field, FieldPatch::Set(value));
        }
        let body: Value = serde_json::from_slice(&patch_body(&patch).unwrap()).unwrap();
        assert_eq!(body["jobTitle"], json!("Programmer"));
        assert_eq!(body["categories"], json!(["friend", "work"]));

        let mut clear = ContactPatch::default();
        for field in [
            ContactField::Name,
            ContactField::Emails,
            ContactField::Phones,
            ContactField::Addresses,
            ContactField::Organizations,
            ContactField::Titles,
            ContactField::Notes,
            ContactField::Keywords,
            ContactField::Urls,
        ] {
            clear.fields.insert(field, FieldPatch::Clear);
        }
        let body: Value = serde_json::from_slice(&patch_body(&clear).unwrap()).unwrap();
        assert!(body["displayName"].is_null());
        assert!(body["categories"].is_null());
        assert!(body.get("urls").is_none());

        let mut unsupported = ContactPatch::default();
        unsupported.fields.insert(
            ContactField::Urls,
            FieldPatch::Set(Value::Array(Vec::new())),
        );
        assert!(patch_body(&unsupported).is_err());
        let mut malformed = ContactPatch::default();
        malformed
            .fields
            .insert(ContactField::Name, FieldPatch::Set(json!("not a name")));
        assert!(patch_body(&malformed).is_err());
    }
}
