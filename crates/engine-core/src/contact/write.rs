//! Provider-neutral contact write intent and field capabilities.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{ContactCard, ContactKind, ContactProperty, PropertyId};
use crate::ids::AddressBookId;

/// A writable normalized contact field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactField {
    /// Card kind.
    Kind,
    /// Structured name.
    Name,
    /// Nicknames.
    Nicknames,
    /// Email addresses.
    Emails,
    /// Phone numbers.
    Phones,
    /// Postal addresses.
    Addresses,
    /// Organizations.
    Organizations,
    /// Titles/roles.
    Titles,
    /// Anniversaries/dates.
    Anniversaries,
    /// Notes.
    Notes,
    /// URLs.
    Urls,
    /// Online services.
    OnlineServices,
    /// Relations.
    Relations,
    /// Languages.
    Languages,
    /// Personal information.
    PersonalInfo,
    /// Calendar links.
    Calendars,
    /// Scheduling addresses.
    SchedulingAddresses,
    /// Crypto keys.
    CryptoKeys,
    /// Directory links.
    Directories,
    /// Keywords/categories.
    Keywords,
    /// Time zone.
    TimeZone,
}

/// An explicit set of fields accepted or requested by a contact operation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContactFieldSet(BTreeSet<ContactField>);

impl ContactFieldSet {
    /// Creates an empty field set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a set from fields.
    #[must_use]
    pub fn from_fields(fields: impl IntoIterator<Item = ContactField>) -> Self {
        Self(fields.into_iter().collect())
    }

    /// Returns whether a field is present.
    #[must_use]
    pub fn contains(&self, field: ContactField) -> bool {
        self.0.contains(&field)
    }

    /// Returns whether every requested field is supported.
    #[must_use]
    pub fn contains_all(&self, requested: &Self) -> bool {
        requested.0.is_subset(&self.0)
    }

    /// Iterates in stable field order.
    pub fn iter(&self) -> impl Iterator<Item = ContactField> + '_ {
        self.0.iter().copied()
    }

    pub(crate) fn from_card(card: &ContactCard) -> Self {
        let mut fields = BTreeSet::from([ContactField::Kind]);
        macro_rules! populated {
            ($value:expr, $field:expr) => {
                if !$value.is_empty() {
                    fields.insert($field);
                }
            };
        }
        if card.name.is_some() {
            fields.insert(ContactField::Name);
        }
        populated!(card.nicknames, ContactField::Nicknames);
        populated!(card.emails, ContactField::Emails);
        populated!(card.phones, ContactField::Phones);
        populated!(card.addresses, ContactField::Addresses);
        populated!(card.organizations, ContactField::Organizations);
        populated!(card.titles, ContactField::Titles);
        populated!(card.anniversaries, ContactField::Anniversaries);
        populated!(card.notes, ContactField::Notes);
        populated!(card.urls, ContactField::Urls);
        populated!(card.online_services, ContactField::OnlineServices);
        populated!(card.relations, ContactField::Relations);
        populated!(card.languages, ContactField::Languages);
        populated!(card.personal_info, ContactField::PersonalInfo);
        populated!(card.calendars, ContactField::Calendars);
        populated!(card.scheduling_addresses, ContactField::SchedulingAddresses);
        populated!(card.crypto_keys, ContactField::CryptoKeys);
        populated!(card.directories, ContactField::Directories);
        if !card.keywords.is_empty() {
            fields.insert(ContactField::Keywords);
        }
        if card.time_zone.is_some() {
            fields.insert(ContactField::TimeZone);
        }
        Self(fields)
    }
}

/// Intent for creating one card in an explicit address book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactDraft {
    /// Destination address book.
    pub address_book: AddressBookId,
    /// Card values. Its provider id and revisions are ignored on create.
    pub card: ContactCard,
}

impl ContactDraft {
    /// Returns the fields requested by the draft.
    #[must_use]
    pub fn requested_fields(&self) -> ContactFieldSet {
        self.card.populated_fields()
    }
}

/// A three-state field patch: leave unchanged, set, or clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldPatch<T> {
    /// Set/replace the field.
    Set(T),
    /// Remove the field.
    Clear,
}

/// Provider-neutral targeted changes to one contact card.
///
/// The explicit map prevents absent fields from being interpreted as clears.
/// Values are JSON-serializable intent because field value types differ; engine
/// APIs validate and adapters decode only fields they advertise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactPatch {
    /// Replacement kind.
    pub kind: Option<FieldPatch<ContactKind>>,
    /// Per-field values keyed by their neutral field.
    pub fields: std::collections::BTreeMap<ContactField, FieldPatch<serde_json::Value>>,
}

impl ContactPatch {
    /// Returns fields this patch requests.
    #[must_use]
    pub fn requested_fields(&self) -> ContactFieldSet {
        let mut fields: BTreeSet<ContactField> = self.fields.keys().copied().collect();
        if self.kind.is_some() {
            fields.insert(ContactField::Kind);
        }
        ContactFieldSet(fields)
    }

    /// Adds a typed property-map replacement.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if `value` cannot be represented as JSON.
    pub fn set_properties<T: Serialize>(
        &mut self,
        field: ContactField,
        value: &std::collections::BTreeMap<PropertyId, ContactProperty<T>>,
    ) -> Result<(), serde_json::Error> {
        self.fields
            .insert(field, FieldPatch::Set(serde_json::to_value(value)?));
        Ok(())
    }
}
