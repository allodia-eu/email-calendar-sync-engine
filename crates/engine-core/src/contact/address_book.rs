//! Discovered contact-source containers.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::ContactSourceClass;
use crate::{
    extended::ExtendedProperties, ids::AddressBookId, raw::RawProviderJson, version::RevisionTokens,
};

/// A discovered provider address book or contact source container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddressBook {
    /// Provider-assigned address-book id.
    pub id: AddressBookId,
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Source authority class.
    pub source_class: ContactSourceClass,
    /// Whether cards in this destination can be written.
    pub is_writable: bool,
    /// Whether this is the provider's default address book.
    pub is_default: bool,
    /// Whether the current user is subscribed to a shared book.
    pub is_subscribed: bool,
    /// Provider owner/principal reference, when exposed.
    pub owner: Option<String>,
    /// Rights/ACL names preserved as an open set.
    pub rights: BTreeSet<String>,
    /// Fields accepted by this destination, retained for diagnostics.
    pub supported_fields: BTreeMap<String, bool>,
    /// Provider revision tokens.
    pub revisions: RevisionTokens,
    /// Provider-defined normalized extension values.
    pub extended: ExtendedProperties,
    /// Original provider JSON, when the provider uses JSON.
    pub raw_provider_json: Option<RawProviderJson>,
}

impl AddressBook {
    /// Creates an address book with conservative read-only defaults.
    #[must_use]
    pub fn new(
        id: AddressBookId,
        name: impl Into<String>,
        source_class: ContactSourceClass,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            description: None,
            source_class,
            is_writable: false,
            is_default: false,
            is_subscribed: true,
            owner: None,
            rights: BTreeSet::new(),
            supported_fields: BTreeMap::new(),
            revisions: RevisionTokens::none(),
            extended: ExtendedProperties::new(),
            raw_provider_json: None,
        }
    }
}

impl Default for AddressBook {
    fn default() -> Self {
        Self::new(
            AddressBookId::try_from("unknown").expect("static non-empty id"),
            "",
            ContactSourceClass::Personal,
        )
    }
}
