//! Serializing a normalized `ContactCard` back to vCard 4.
//!
//! Split from the parsing half so each direction sits with its own tests, and to keep
//! both files under the line limit. The two halves share `vcard_escape`, and their
//! agreement is a real invariant: what `build_vcard`/`patch_vcard` emit here must be
//! exactly what `parse_vcard` recovers — the `N` line especially, whose two nested
//! separator levels are escape-aware on both sides.

use engine_core::contact::{
    ContactCard, ContactEmail, ContactKind, ContactName, ContactNote, ContactPatch, ContactPhone,
    ContactProperty, ContactResource, FieldPatch, PropertyId,
};

use crate::{
    error::CalDavError,
    vcard::{NAME_COMPONENT_ORDER, unfold},
    vcard_escape::escape,
};

/// Serializes the components a `N` line can carry, or `None` when the name has none.
///
/// Written whenever `FN` is, because both writers *remove* any `N` the card had: a
/// name edit that emitted only `FN` would delete the structured name from the server
/// card, and the next sync would read the contact back without components. Kinds
/// outside RFC 6350's five slots (`Surname2`, `Other`) have nowhere to go in vCard
/// and are dropped rather than mis-filed.
fn structured_name_line(name: &ContactName) -> Option<String> {
    if name.components.is_empty() {
        return None;
    }
    let fields: Vec<String> = NAME_COMPONENT_ORDER
        .iter()
        .map(|kind| {
            name.components
                .iter()
                .filter(|component| &component.kind == kind)
                .map(|component| escape(&component.value))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();
    fields
        .iter()
        .any(|field| !field.is_empty())
        .then(|| format!("N:{}", fields.join(";")))
}

pub(crate) fn build_vcard(card: &ContactCard) -> String {
    let mut lines = vec!["BEGIN:VCARD".into(), "VERSION:4.0".into()];
    if let Some(uid) = &card.uid {
        lines.push(format!("UID:{}", escape(uid)));
    }
    lines.push(format!("KIND:{}", escape(kind_text(&card.kind))));
    if let Some(name) = card.display_name() {
        lines.push(format!("FN:{}", escape(&name)));
    }
    if let Some(line) = card.name.as_ref().and_then(structured_name_line) {
        lines.push(line);
    }
    for email in card.emails.values() {
        lines.push(format!("EMAIL:{}", escape(&email.value.address)));
    }
    for phone in card.phones.values() {
        lines.push(format!("TEL:{}", escape(&phone.value.number)));
    }
    for note in card.notes.values() {
        lines.push(format!("NOTE:{}", escape(&note.value.note)));
    }
    for url in card.urls.values() {
        lines.push(format!("URL:{}", escape(&url.value.uri)));
    }
    if !card.keywords.is_empty() {
        lines.push(format!(
            "CATEGORIES:{}",
            card.keywords
                .iter()
                .map(|value| escape(value))
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    lines.push("END:VCARD".into());
    format!("{}\r\n", lines.join("\r\n"))
}

pub(crate) fn patch_vcard(base: &ContactCard, patch: &ContactPatch) -> Result<String, CalDavError> {
    let raw = base
        .raw_vcard
        .as_ref()
        .ok_or_else(|| CalDavError::protocol("CardDAV patch requires raw vCard"))?;
    let mut lines = unfold(raw.as_str());
    for (field, edit) in &patch.fields {
        let names: &[&str] = match field {
            engine_core::contact::ContactField::Name => &["FN", "N"],
            engine_core::contact::ContactField::Emails => &["EMAIL"],
            engine_core::contact::ContactField::Phones => &["TEL"],
            engine_core::contact::ContactField::Notes => &["NOTE"],
            engine_core::contact::ContactField::Urls => &["URL"],
            engine_core::contact::ContactField::Keywords => &["CATEGORIES"],
            _ => {
                return Err(CalDavError::protocol(format!(
                    "unsupported CardDAV contact patch field {field:?}"
                )));
            }
        };
        lines.retain(|line| {
            line.split_once(':').is_none_or(|(head, _)| {
                let property = head
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .rsplit('.')
                    .next()
                    .unwrap_or_default();
                !names.iter().any(|name| property.eq_ignore_ascii_case(name))
            })
        });
        if let FieldPatch::Set(value) = edit {
            insert_patch_lines(&mut lines, *field, value)?;
        }
    }
    if let Some(kind) = &patch.kind {
        lines.retain(|line| {
            !line
                .split_once(':')
                .is_some_and(|(head, _)| head.eq_ignore_ascii_case("KIND"))
        });
        if let FieldPatch::Set(kind) = kind {
            insert_before_end(&mut lines, format!("KIND:{}", escape(kind_text(kind))));
        }
    }
    Ok(format!("{}\r\n", lines.join("\r\n")))
}

fn insert_patch_lines(
    lines: &mut Vec<String>,
    field: engine_core::contact::ContactField,
    value: &serde_json::Value,
) -> Result<(), CalDavError> {
    use engine_core::contact::ContactField;
    match field {
        ContactField::Name => {
            let name: ContactName = decode(value)?;
            if let Some(display) = name.display() {
                insert_before_end(lines, format!("FN:{}", escape(&display)));
            }
            // `patch_vcard` removed the card's `N` along with its `FN`; the edit owns
            // both halves of the name, so it has to write both back.
            if let Some(line) = structured_name_line(&name) {
                insert_before_end(lines, line);
            }
        }
        ContactField::Emails => {
            let values: std::collections::BTreeMap<PropertyId, ContactProperty<ContactEmail>> =
                decode(value)?;
            for email in values.values() {
                insert_before_end(lines, format!("EMAIL:{}", escape(&email.value.address)));
            }
        }
        ContactField::Phones => {
            let values: std::collections::BTreeMap<PropertyId, ContactProperty<ContactPhone>> =
                decode(value)?;
            for phone in values.values() {
                insert_before_end(lines, format!("TEL:{}", escape(&phone.value.number)));
            }
        }
        ContactField::Notes => {
            let values: std::collections::BTreeMap<PropertyId, ContactProperty<ContactNote>> =
                decode(value)?;
            for note in values.values() {
                insert_before_end(lines, format!("NOTE:{}", escape(&note.value.note)));
            }
        }
        ContactField::Urls => {
            let values: std::collections::BTreeMap<PropertyId, ContactProperty<ContactResource>> =
                decode(value)?;
            for url in values.values() {
                insert_before_end(lines, format!("URL:{}", escape(&url.value.uri)));
            }
        }
        ContactField::Keywords => {
            let values: std::collections::BTreeSet<String> = decode(value)?;
            insert_before_end(
                lines,
                format!(
                    "CATEGORIES:{}",
                    values
                        .iter()
                        .map(|value| escape(value))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            );
        }
        _ => {}
    }
    Ok(())
}

fn insert_before_end(lines: &mut Vec<String>, line: String) {
    let position = lines
        .iter()
        .position(|value| value.eq_ignore_ascii_case("END:VCARD"))
        .unwrap_or(lines.len());
    lines.insert(position, line);
}

fn decode<T: serde::de::DeserializeOwned>(value: &serde_json::Value) -> Result<T, CalDavError> {
    serde_json::from_value(value.clone()).map_err(|error| CalDavError::protocol(error.to_string()))
}

fn kind_text(kind: &ContactKind) -> &str {
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
