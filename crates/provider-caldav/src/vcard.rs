//! Raw-preserving vCard 3/4 normalization for CardDAV.

use engine_core::{
    contact::{
        Anniversary, ContactAddress, ContactCard, ContactEmail, ContactKind, ContactMember,
        ContactName, ContactNickname, ContactNote, ContactPhone, ContactProperty, ContactResource,
        NameComponent, NameComponentKind, Organization, OrganizationUnit, PropertyId, Title,
    },
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::RawVcard,
};

use crate::{
    error::CalDavError,
    vcard_escape::{split_escaped_list, split_escaped_raw, unescape},
    vcard_property::property_id,
};

pub(crate) fn parse_vcard(
    raw: &str,
    id: ContactId,
    address_book: AddressBookId,
    writable: bool,
) -> Result<ContactCard, CalDavError> {
    let lines = unfold(raw);
    if !lines
        .iter()
        .any(|line| line.eq_ignore_ascii_case("BEGIN:VCARD"))
        || !lines
            .iter()
            .any(|line| line.eq_ignore_ascii_case("END:VCARD"))
    {
        return Err(CalDavError::protocol("vCard is missing BEGIN/END"));
    }
    let mut card = ContactCard::new(id, Memberships::of_one(address_book));
    card.is_writable = writable;
    for (index, line) in lines.iter().enumerate() {
        let Some((head, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = head.split(';');
        let property = parts
            .next()
            .and_then(|name| name.rsplit('.').next())
            .unwrap_or_default()
            .to_ascii_uppercase();
        let parameters: Vec<&str> = parts.collect();
        match property.as_str() {
            "UID" => card.uid = Some(unescape(value)),
            "KIND" => card.kind = kind(value),
            "FN" => {
                card.name.get_or_insert_with(ContactName::default).full = Some(unescape(value));
            }
            "N" => structured_name(&mut card, value),
            "NICKNAME" => {
                card.nicknames.insert(
                    property_id(&parameters, "nickname", index)?,
                    decorated(ContactNickname::new(unescape(value)), &parameters),
                );
            }
            "EMAIL" => {
                card.emails.insert(
                    property_id(&parameters, "email", index)?,
                    decorated(ContactEmail::new(unescape(value)), &parameters),
                );
            }
            "TEL" => {
                let mut phone = ContactPhone {
                    number: unescape(value),
                    ..ContactPhone::default()
                };
                phone.features.extend(
                    types(&parameters)
                        .filter(|kind| !matches!(kind.as_str(), "home" | "work" | "private")),
                );
                card.phones.insert(
                    property_id(&parameters, "phone", index)?,
                    decorated(phone, &parameters),
                );
            }
            "ADR" => {
                card.addresses.insert(
                    property_id(&parameters, "address", index)?,
                    decorated(address(value), &parameters),
                );
            }
            "ORG" => organization(&mut card, value, &parameters, index)?,
            "TITLE" | "ROLE" => {
                card.titles.insert(
                    property_id(&parameters, "title", index)?,
                    decorated(
                        Title {
                            name: unescape(value),
                            kind: Some(property.to_ascii_lowercase()),
                            ..Title::default()
                        },
                        &parameters,
                    ),
                );
            }
            "BDAY" | "ANNIVERSARY" => {
                card.anniversaries.insert(
                    property_id(&parameters, "anniversary", index)?,
                    decorated(
                        Anniversary {
                            date: unescape(value),
                            kind: Some(if property == "BDAY" {
                                "birth".into()
                            } else {
                                "wedding".into()
                            }),
                            place: None,
                        },
                        &parameters,
                    ),
                );
            }
            "NOTE" => {
                card.notes.insert(
                    property_id(&parameters, "note", index)?,
                    decorated(ContactNote::new(unescape(value)), &parameters),
                );
            }
            "URL" => resource(&mut card.urls, value, &parameters, "url", index)?,
            "PHOTO" | "LOGO" | "SOUND" => {
                resource(
                    &mut card.media,
                    value,
                    &parameters,
                    &property.to_ascii_lowercase(),
                    index,
                )?;
            }
            "MEMBER" => {
                card.members.insert(
                    property_id(&parameters, "member", index)?,
                    decorated(ContactMember::new(unescape(value)), &parameters),
                );
            }
            "CATEGORIES" => {
                card.keywords.extend(split_escaped_list(value, ','));
            }
            "TZ" => card.time_zone = Some(unescape(value)),
            _ => {}
        }
    }
    card.raw_vcard = Some(RawVcard::new(raw));
    Ok(card)
}

pub(crate) fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").split('\n') {
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(previous) = lines.last_mut()
        {
            previous.push_str(&line[1..]);
        } else if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines
}

fn kind(value: &str) -> ContactKind {
    match value.trim().to_ascii_lowercase().as_str() {
        "individual" => ContactKind::Individual,
        "org" => ContactKind::Organization,
        "group" => ContactKind::Group,
        "location" => ContactKind::Location,
        "device" => ContactKind::Device,
        "application" => ContactKind::Application,
        other => ContactKind::Other(other.to_owned()),
    }
}

/// RFC 6350 §6.2.2 orders `N` as family, given, additional, prefixes, suffixes.
pub(crate) const NAME_COMPONENT_ORDER: [NameComponentKind; 5] = [
    NameComponentKind::Surname,
    NameComponentKind::Given,
    NameComponentKind::Middle,
    NameComponentKind::Prefix,
    NameComponentKind::Suffix,
];

fn structured_name(card: &mut ContactCard, value: &str) {
    let name = card.name.get_or_insert_with(ContactName::default);
    // Two nested separators: `;` between slots, `,` between values within one slot
    // (RFC 6350 §6.2.2). Both honour escaping, so split raw and unescape at the leaf.
    let fields = split_escaped_raw(value, ';');
    for (index, kind) in NAME_COMPONENT_ORDER.into_iter().enumerate() {
        let Some(field) = fields.get(index) else {
            continue;
        };
        for value in split_escaped_list(field, ',') {
            if !value.is_empty() {
                name.components
                    .push(NameComponent::new(kind.clone(), value));
            }
        }
    }
}

fn decorated<T>(value: T, parameters: &[&str]) -> ContactProperty<T> {
    let mut property = ContactProperty::new(value);
    property.contexts = types(parameters)
        .filter_map(|kind| match kind.as_str() {
            "home" => Some("private".into()),
            "work" => Some("work".into()),
            _ => None,
        })
        .collect();
    property.preference = parameters.iter().find_map(|parameter| {
        parameter
            .strip_prefix("PREF=")
            .or_else(|| parameter.strip_prefix("pref="))
            .and_then(|value| value.parse().ok())
    });
    property
}

fn types<'a>(parameters: &'a [&'a str]) -> impl Iterator<Item = String> + 'a {
    parameters.iter().flat_map(|parameter| {
        parameter
            .strip_prefix("TYPE=")
            .or_else(|| parameter.strip_prefix("type="))
            .unwrap_or_default()
            .split(',')
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase)
    })
}

fn address(value: &str) -> ContactAddress {
    let fields: Vec<String> = value.split(';').map(unescape).collect();
    let mut address = ContactAddress::default();
    for (index, key) in [
        "po_box", "extended", "street", "locality", "region", "postcode", "country",
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(value) = fields.get(index).filter(|value| !value.is_empty()) {
            address.components.insert(key.into(), vec![value.clone()]);
        }
    }
    address
}

fn organization(
    card: &mut ContactCard,
    value: &str,
    parameters: &[&str],
    index: usize,
) -> Result<(), CalDavError> {
    let mut parts = value.split(';').map(unescape);
    let organization = Organization {
        name: parts.next().unwrap_or_default(),
        units: parts
            .filter(|part| !part.is_empty())
            .map(|name| OrganizationUnit {
                name,
                ..OrganizationUnit::default()
            })
            .collect(),
        ..Organization::default()
    };
    card.organizations.insert(
        property_id(parameters, "organization", index)?,
        ContactProperty::new(organization),
    );
    Ok(())
}

fn resource(
    target: &mut std::collections::BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    value: &str,
    parameters: &[&str],
    kind: &str,
    index: usize,
) -> Result<(), CalDavError> {
    let mut media_type = parameters.iter().find_map(|parameter| {
        parameter
            .strip_prefix("MEDIATYPE=")
            .or_else(|| parameter.strip_prefix("mediatype="))
            .map(str::to_owned)
    });
    let encoded = parameters.iter().any(|parameter| {
        parameter.split_once('=').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("encoding")
                && matches!(value.to_ascii_lowercase().as_str(), "b" | "base64")
        })
    });
    if media_type.is_none() && encoded {
        media_type = parameters.iter().find_map(|parameter| {
            let (key, value) = parameter.split_once('=')?;
            key.eq_ignore_ascii_case("type")
                .then(|| format!("image/{}", value.to_ascii_lowercase()))
        });
    }
    let uri = if encoded {
        format!(
            "data:{};base64,{value}",
            media_type.as_deref().unwrap_or("application/octet-stream")
        )
    } else {
        unescape(value)
    };
    target.insert(
        property_id(parameters, kind, index)?,
        decorated(
            ContactResource {
                uri,
                kind: Some(kind.to_owned()),
                media_type,
                title: None,
                fingerprint: None,
            },
            parameters,
        ),
    );
    Ok(())
}
