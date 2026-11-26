//! JMAP ContactCard create objects and PatchObjects.

use engine_core::contact::{
    ContactCard, ContactDraft, ContactField, ContactKind, ContactPatch, FieldPatch,
};
use serde_json::{Map, Value, json};

use crate::{contact_write_fields, error::JmapError};

/// The JSContact properties [`contact_write_fields::card_object`] is authoritative
/// for. Anything else a stored raw card carries — a vendor `x-` extension, a property
/// added to JSContact after this version — is not ours to rewrite, so it rides along.
const MODELLED_PROPERTIES: &[&str] = &[
    "@type",
    "version",
    "kind",
    "uid",
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
];

/// The `ContactCard/set` create object for `draft`: [`writable_object`], its book, and always a
/// `uid`.
///
/// RFC 9553 §2.1.9 makes a Card's `uid` mandatory, and RFC 9610 does not ask a server to supply a
/// missing one (RFC 8620 §5.3 lets it set a default for "any such property [that] may be omitted
/// by the client", and the `uid` has none), so a create never leaves it out: a draft without its
/// own gets [`engine_provider::content_uid_of`], minted from the whole draft. That is a fallback,
/// not an identity: a retried create of the same draft sends the same `uid`, which RFC 9610 §3
/// forbids a second card to hold ("there MUST NOT be more than one ContactCard with the same uid
/// in an Account"), but an edited draft sends another. Stalwart stores a Card sent without a
/// `uid` as it is, with none (`docs/spec-deviations.md`, STALWART-11).
pub(crate) fn create_object(draft: &ContactDraft) -> Map<String, Value> {
    let mut object = writable_object(&draft.card);
    object.insert(
        "addressBookIds".into(),
        json!({ draft.address_book.as_str(): true }),
    );
    if !object.contains_key("uid") {
        object.insert(
            "uid".into(),
            json!(engine_provider::content_uid_of(draft).uid().as_str()),
        );
    }
    object
}

/// Builds the `ContactCard/set` **create** object for a card, without its book.
///
/// The card's own values always win. Returning the stored raw JSContact verbatim was
/// tempting — it is byte-faithful to what the server last sent — but a create is not a
/// re-upload: the caller reached this path *because* it built or edited a card, and a
/// host that clones a fetched card, changes an address, and creates the copy would
/// have shipped the original address. So the raw object contributes only the
/// properties this engine does not model, and every modelled property is re-derived
/// from the card — including the ones the host emptied, which must stay empty.
pub(crate) fn writable_object(card: &ContactCard) -> Map<String, Value> {
    let mut object = card
        .raw_jscontact
        .as_ref()
        .and_then(|raw| serde_json::from_str::<Map<String, Value>>(raw.as_str()).ok())
        .unwrap_or_default();
    // The server assigns the id; a create must not name one.
    object.remove("id");
    object.retain(|name, _| !MODELLED_PROPERTIES.contains(&name.as_str()));
    object.extend(contact_write_fields::card_object(card));
    object
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
