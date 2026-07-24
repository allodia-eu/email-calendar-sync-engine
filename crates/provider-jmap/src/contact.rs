//! JMAP Contacts (RFC 9610) normalization and provider contract.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactEmail, ContactField, ContactFieldSet,
        ContactKind, ContactMember, ContactName, ContactPatch, ContactProperty, ContactResource,
        ContactSourceClass, NameComponent, NameComponentKind, PropertyId,
    },
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    raw::{RawJsContact, RawProviderJson},
    sync::SyncState,
};
use engine_provider::{
    ContactDestination, ContactPhoto, ContactSourceSync, ContactWriteReceipt, ContactsProvider,
    Provider, ProviderResult,
};
use serde_json::{Value, json};

use crate::{
    JmapProvider, contact_fields, contact_write, error::JmapError, fetch, request::capability,
};

const CREATION_ID: &str = "new";

pub(crate) fn address_book(value: &Value) -> Result<AddressBook, JmapError> {
    let id = address_book_id(required_text(value, "id")?)?;
    let mut book = AddressBook::new(
        id,
        value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Address book"),
        ContactSourceClass::Personal,
    );
    book.description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    book.is_default = value
        .get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    book.is_subscribed = value
        .get("isSubscribed")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let rights = value.get("myRights").and_then(Value::as_object);
    book.is_writable = rights
        .and_then(|rights| rights.get("mayWrite"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(rights) = rights {
        book.rights.extend(
            rights
                .iter()
                .filter(|(_, allowed)| allowed.as_bool() == Some(true))
                .map(|(name, _)| name.clone()),
        );
    }
    book.raw_provider_json = Some(RawProviderJson::new(value.to_string()));
    Ok(book)
}

pub(crate) fn card(value: &Value) -> Result<ContactCard, JmapError> {
    let id = ContactId::try_from(required_text(value, "id")?)
        .map_err(|error| JmapError::protocol(error.to_string()))?;
    let books = value
        .get("addressBookIds")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|values| values.iter())
        .filter(|(_, present)| present.as_bool() == Some(true))
        .map(|(id, _)| address_book_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let memberships =
        Memberships::new(books).map_err(|error| JmapError::protocol(error.to_string()))?;
    let mut card = ContactCard::new(id, memberships);
    card.uid = value.get("uid").and_then(Value::as_str).map(str::to_owned);
    card.kind = match value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("individual")
    {
        "individual" => ContactKind::Individual,
        "org" | "organization" => ContactKind::Organization,
        "group" => ContactKind::Group,
        "location" => ContactKind::Location,
        "device" => ContactKind::Device,
        "application" => ContactKind::Application,
        other => ContactKind::Other(other.to_owned()),
    };
    card.name = normalize_name(value.get("name"));
    card.emails = property_map(value.get("emails"), |entry| {
        entry
            .get("address")
            .and_then(Value::as_str)
            .map(|address| ContactEmail::new(address.to_owned()))
    })?;
    card.members = members(value.get("members"))?;
    card.media = property_map(value.get("media"), |entry| {
        Some(ContactResource {
            uri: entry.get("uri")?.as_str()?.to_owned(),
            kind: entry.get("kind").and_then(Value::as_str).map(str::to_owned),
            media_type: entry
                .get("mediaType")
                .and_then(Value::as_str)
                .map(str::to_owned),
            title: entry
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            fingerprint: entry
                .get("blobId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    })?;
    card.source_class = ContactSourceClass::Personal;
    card.is_writable = value
        .get("isReadOnly")
        .and_then(Value::as_bool)
        .is_none_or(|read_only| !read_only);
    contact_fields::apply(&mut card, value)?;
    card.raw_jscontact = Some(RawJsContact::new(value.to_string()));
    Ok(card)
}

fn normalize_name(value: Option<&Value>) -> Option<ContactName> {
    let value = value?.as_object()?;
    let mut name = ContactName {
        full: value.get("full").and_then(Value::as_str).map(str::to_owned),
        ..ContactName::default()
    };
    for component in value
        .get("components")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = match component.get("kind").and_then(Value::as_str)? {
            "title" => NameComponentKind::Prefix,
            "given" => NameComponentKind::Given,
            "given2" => NameComponentKind::Middle,
            "surname" => NameComponentKind::Surname,
            "surname2" => NameComponentKind::Surname2,
            "credential" => NameComponentKind::Suffix,
            other => NameComponentKind::Other(other.to_owned()),
        };
        name.components
            .push(NameComponent::new(kind, component.get("value")?.as_str()?));
    }
    Some(name)
}

/// Normalizes a group Card's `members`.
///
/// RFC 9553 §2.1.7 types this as `String[Boolean]`: **each key is a member Card's
/// `uid`** and each value MUST be `true`. There is no `Member` object, so — unlike
/// every other property map — the uid cannot be read out of the entry, and
/// [`property_map`] does not apply. Reading it as an object with a `uid` field
/// silently yields *no members* against a real server.
fn members(
    value: Option<&Value>,
) -> Result<BTreeMap<PropertyId, ContactProperty<ContactMember>>, JmapError> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        // A value other than `true` is not a membership; the spec admits only `true`.
        .filter(|(_, flag)| flag.as_bool() == Some(true))
        .map(|(uid, _)| {
            Ok((
                property_id(uid)?,
                ContactProperty::new(ContactMember::new(uid.clone())),
            ))
        })
        .collect()
}

fn property_map<T>(
    value: Option<&Value>,
    normalize: impl Fn(&Value) -> Option<T>,
) -> Result<BTreeMap<PropertyId, ContactProperty<T>>, JmapError> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(|(id, entry)| normalize(entry).map(|normalized| (id, entry, normalized)))
        .map(|(id, entry, normalized)| {
            let mut property = ContactProperty::new(normalized);
            property.contexts = true_keys(entry.get("contexts"));
            property.preference = entry
                .get("pref")
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok());
            property.label = entry
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Ok((property_id(id)?, property))
        })
        .collect()
}

fn true_keys(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(_, present)| present.as_bool() == Some(true))
        .map(|(name, _)| name.clone())
        .collect()
}

#[async_trait]
impl ContactsProvider for JmapProvider {
    fn contact_destination(&self) -> Option<ContactDestination> {
        self.connection_info()
            .capabilities
            .contact_write_guard()
            .map(|guard| ContactDestination {
                address_book: self.contact_address_book.clone(),
                source_class: ContactSourceClass::Personal,
                writable: true,
                write_guard: Some(guard),
                supported_fields: supported_fields(),
            })
    }

    async fn sync_address_books(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let account = self.contact_account()?;
        let (sync, cursor_recovered) = fetch::container_sync_with_status(
            self.executor(),
            &account,
            &[capability::CORE, capability::CONTACTS],
            "AddressBook",
            cursor,
            address_book,
            |book| book.id.key().clone(),
        )
        .await?;
        Ok(ContactSourceSync::Available {
            sync,
            cursor_recovered,
        })
    }

    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        let account = self.contact_account()?;
        let (sync, cursor_recovered) = fetch::container_sync_with_status(
            self.executor(),
            &account,
            &[capability::CORE, capability::CONTACTS],
            "ContactCard",
            cursor,
            card,
            |card| card.id.key().clone(),
        )
        .await?;
        Ok(ContactSourceSync::Available {
            sync,
            cursor_recovered,
        })
    }

    async fn fetch_contact(
        &self,
        _account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        let result = self
            .contact_call(
                "ContactCard/get",
                json!({
                    "accountId": self.contact_account()?,
                    "ids": [contact.as_str()]
                }),
            )
            .await?;
        let value = result
            .get("list")
            .and_then(Value::as_array)
            .and_then(|list| list.first())
            .ok_or_else(|| JmapError::protocol("ContactCard/get returned no card"))?;
        Ok(card(value)?)
    }

    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        let mut object = contact_write::writable_object(&draft.card);
        object.insert(
            "addressBookIds".into(),
            json!({ draft.address_book.as_str(): true }),
        );
        let result = self
            .contact_call(
                "ContactCard/set",
                json!({
                    "accountId": self.contact_account()?,
                    "create": { CREATION_ID: object }
                }),
            )
            .await?;
        check_set_error(&result, "notCreated", CREATION_ID)?;
        let id = result
            .get("created")
            .and_then(|created| created.get(CREATION_ID))
            .and_then(|created| created.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| JmapError::set(CREATION_ID, "notFound"))?;
        Ok(ContactWriteReceipt::new(contact_id(id)?))
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        let object = contact_write::patch_object(patch)?;
        if object.is_empty() {
            return Ok(ContactWriteReceipt::new(base.id.clone()));
        }
        let result = self
            .contact_call(
                "ContactCard/set",
                json!({
                    "accountId": self.contact_account()?,
                    "update": { base.id.as_str(): object }
                }),
            )
            .await?;
        check_set_error(&result, "notUpdated", base.id.as_str())?;
        Ok(ContactWriteReceipt::new(base.id.clone()))
    }

    async fn delete_contact(&self, _account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        let result = self
            .contact_call(
                "ContactCard/set",
                json!({
                    "accountId": self.contact_account()?,
                    "destroy": [base.id.as_str()]
                }),
            )
            .await?;
        if let Some(kind) = set_error(&result, "notDestroyed", base.id.as_str())
            && kind != "notFound"
        {
            return Err(JmapError::set(base.id.as_str(), kind).into());
        }
        Ok(())
    }

    async fn fetch_contact_photo(
        &self,
        _account: &AccountId,
        _card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<ContactPhoto> {
        let blob = media
            .fingerprint
            .as_deref()
            .ok_or_else(|| JmapError::protocol("JMAP media has no blobId"))?;
        let bytes = self
            .download_contact_blob(blob, media.media_type.as_deref())
            .await?;
        Ok(ContactPhoto::new(
            bytes,
            media.media_type.clone(),
            blob.to_owned(),
        ))
    }
}

fn supported_fields() -> ContactFieldSet {
    ContactFieldSet::from_fields([
        ContactField::Kind,
        ContactField::Name,
        ContactField::Nicknames,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Addresses,
        ContactField::Organizations,
        ContactField::Titles,
        ContactField::Anniversaries,
        ContactField::Notes,
        ContactField::Urls,
        ContactField::OnlineServices,
        ContactField::Relations,
        ContactField::Languages,
        ContactField::PersonalInfo,
        ContactField::Calendars,
        ContactField::SchedulingAddresses,
        ContactField::CryptoKeys,
        ContactField::Directories,
        ContactField::Keywords,
    ])
}

fn check_set_error(result: &Value, map: &str, target: &str) -> Result<(), JmapError> {
    if let Some(kind) = set_error(result, map, target) {
        Err(JmapError::set(target, kind))
    } else {
        Ok(())
    }
}

fn set_error<'a>(result: &'a Value, map: &str, target: &str) -> Option<&'a str> {
    result
        .get(map)
        .and_then(|errors| errors.get(target))
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
}

fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str, JmapError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::protocol(format!("contact {field} missing")))
}

fn address_book_id(value: &str) -> Result<AddressBookId, JmapError> {
    AddressBookId::try_from(value).map_err(|error| JmapError::protocol(error.to_string()))
}

fn property_id(value: &str) -> Result<PropertyId, JmapError> {
    PropertyId::new(value).map_err(|error| JmapError::protocol(error.to_string()))
}

fn contact_id(value: &str) -> Result<ContactId, JmapError> {
    ContactId::try_from(value).map_err(|error| JmapError::protocol(error.to_string()))
}
