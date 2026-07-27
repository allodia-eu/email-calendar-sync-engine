//! Address-book and contact-card source records.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{ContactFieldSet, ContactProperty, PropertyId};
use crate::{
    extended::ExtendedProperties,
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    raw::{RawJsContact, RawProviderJson, RawVcard},
    time::UtcDateTime,
    version::RevisionTokens,
};

/// The provenance/authority class of a contact source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactSourceClass {
    /// A user's saved personal contacts.
    Personal,
    /// A read-only personal/suggested source such as Google Other Contacts.
    Suggested,
    /// An organization or provider directory.
    Directory,
    /// A synthetic source derived from observed sent mail.
    MailHistory,
}

/// A normalized contact/address-book card kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactKind {
    /// A person.
    #[default]
    Individual,
    /// An organization.
    Organization,
    /// A contact group/distribution list.
    Group,
    /// A physical place.
    Location,
    /// A device.
    Device,
    /// An application/service identity.
    Application,
    /// An extension kind preserved by name.
    Other(String),
}

/// A component in a structured contact name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameComponentKind {
    /// Honorific prefix.
    Prefix,
    /// Given/first name.
    Given,
    /// Middle/additional name.
    Middle,
    /// Surname/family name.
    Surname,
    /// Secondary surname.
    Surname2,
    /// Generation/credential suffix.
    Suffix,
    /// An extension component.
    Other(String),
}

/// One structured name component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameComponent {
    /// Component kind.
    pub kind: NameComponentKind,
    /// Component text.
    pub value: String,
}

impl NameComponent {
    /// Creates a name component.
    #[must_use]
    pub fn new(kind: NameComponentKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

/// A structured contact name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactName {
    /// Provider-formatted full name.
    pub full: Option<String>,
    /// Ordered name components.
    pub components: Vec<NameComponent>,
    /// Sort strings keyed by component kind/name.
    pub sort_as: BTreeMap<String, String>,
    /// Phonetic transcription system, when declared.
    pub phonetic_system: Option<String>,
    /// Unknown name data.
    pub extensions: ExtendedProperties,
}

impl ContactName {
    /// Returns a deterministic human-readable name.
    #[must_use]
    pub fn display(&self) -> Option<String> {
        self.full
            .as_ref()
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .or_else(|| {
                let joined = self
                    .components
                    .iter()
                    .map(|component| component.value.trim())
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                (!joined.is_empty()).then_some(joined)
            })
    }
}

macro_rules! string_value {
    ($(#[$meta:meta])* $name:ident, $field:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord,
            Serialize, Deserialize,
        )]
        pub struct $name {
            /// The normalized value.
            pub $field: String,
        }

        impl $name {
            /// Creates the value.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self { $field: value.into() }
            }
        }
    };
}

string_value! {
    /// A contact email address.
    ContactEmail, address
}
string_value! {
    /// A contact nickname.
    ContactNickname, name
}
string_value! {
    /// A contact note.
    ContactNote, note
}
string_value! {
    /// A preferred spoken/written language tag.
    ContactLanguage, language
}
string_value! {
    /// A group member reference (normally another card UID).
    ContactMember, uid
}

/// A contact phone endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactPhone {
    /// Number or `tel:` URI.
    pub number: String,
    /// Open JSContact features such as `mobile`, `voice`, or `text`.
    pub features: BTreeSet<String>,
}

/// A postal address.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactAddress {
    /// Provider-formatted address.
    pub full: Option<String>,
    /// Structured components keyed by their JSContact/vCard meaning.
    pub components: BTreeMap<String, Vec<String>>,
    /// ISO country code.
    pub country_code: Option<String>,
    /// `geo:` URI.
    pub coordinates: Option<String>,
    /// IANA time-zone id.
    pub time_zone: Option<String>,
    /// Unknown address data.
    pub extensions: ExtendedProperties,
}

/// One nested organizational unit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrganizationUnit {
    /// Unit name.
    pub name: String,
    /// Unit ordering/sort index.
    pub sort_as: Option<String>,
    /// Unknown unit data.
    pub extensions: ExtendedProperties,
}

/// An organization affiliation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Organization {
    /// Organization name.
    pub name: String,
    /// Nested departments/divisions.
    pub units: Vec<OrganizationUnit>,
    /// Unknown organization data.
    pub extensions: ExtendedProperties,
}

/// A title or role.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Title {
    /// Title/role text.
    pub name: String,
    /// `title`, `role`, or an extension kind.
    pub kind: Option<String>,
    /// Referenced organization property id.
    pub organization_id: Option<PropertyId>,
}

/// An anniversary/date value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Anniversary {
    /// JSContact date text, preserving partial/unknown date forms.
    pub date: String,
    /// `birth`, `death`, `wedding`, or an extension kind.
    pub kind: Option<String>,
    /// Associated place text.
    pub place: Option<String>,
}

/// A URI-backed resource such as a URL, calendar, media item, or crypto key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactResource {
    /// Resource URI.
    pub uri: String,
    /// Resource kind such as `photo` or `logo`.
    pub kind: Option<String>,
    /// Media type, when known.
    pub media_type: Option<String>,
    /// Human-readable title.
    pub title: Option<String>,
    /// Provider media/blob fingerprint used for cache invalidation.
    pub fingerprint: Option<String>,
}

/// An online service identity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactOnlineService {
    /// Service name.
    pub service: Option<String>,
    /// User name/handle.
    pub user: Option<String>,
    /// Profile URI.
    pub uri: Option<String>,
}

/// A relation to another person/card.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactRelation {
    /// Relation names such as `friend` or `manager`.
    pub relation: BTreeSet<String>,
    /// Related card UID, when known.
    pub uid: Option<String>,
    /// Related URI, when known.
    pub uri: Option<String>,
}

/// A JSContact personal-information entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonalInfo {
    /// Information kind.
    pub kind: String,
    /// Information value.
    pub value: String,
}

/// A provider contact record normalized on JSContact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContactCard {
    /// Provider object id.
    pub id: ContactId,
    /// Portable JSContact/vCard UID, when supplied.
    pub uid: Option<String>,
    /// Non-empty address-book membership.
    pub address_books: Memberships<AddressBookId>,
    /// Authority class of the source that supplied this record.
    pub source_class: ContactSourceClass,
    /// Whether this exact source record is writable.
    pub is_writable: bool,
    /// Card kind.
    pub kind: ContactKind,
    /// Structured name.
    pub name: Option<ContactName>,
    /// Nicknames.
    pub nicknames: BTreeMap<PropertyId, ContactProperty<ContactNickname>>,
    /// Email endpoints.
    pub emails: BTreeMap<PropertyId, ContactProperty<ContactEmail>>,
    /// Phone endpoints.
    pub phones: BTreeMap<PropertyId, ContactProperty<ContactPhone>>,
    /// Postal addresses.
    pub addresses: BTreeMap<PropertyId, ContactProperty<ContactAddress>>,
    /// Organization affiliations.
    pub organizations: BTreeMap<PropertyId, ContactProperty<Organization>>,
    /// Titles and roles.
    pub titles: BTreeMap<PropertyId, ContactProperty<Title>>,
    /// Anniversaries and dates.
    pub anniversaries: BTreeMap<PropertyId, ContactProperty<Anniversary>>,
    /// Notes.
    pub notes: BTreeMap<PropertyId, ContactProperty<ContactNote>>,
    /// General links.
    pub urls: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Photos, logos, sounds, and other media.
    pub media: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Online service identities.
    pub online_services: BTreeMap<PropertyId, ContactProperty<ContactOnlineService>>,
    /// Relations.
    pub relations: BTreeMap<PropertyId, ContactProperty<ContactRelation>>,
    /// Preferred languages.
    pub languages: BTreeMap<PropertyId, ContactProperty<ContactLanguage>>,
    /// Group members.
    pub members: BTreeMap<PropertyId, ContactProperty<ContactMember>>,
    /// Personal information.
    pub personal_info: BTreeMap<PropertyId, ContactProperty<PersonalInfo>>,
    /// Calendar URIs.
    pub calendars: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Scheduling addresses.
    pub scheduling_addresses: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Crypto keys.
    pub crypto_keys: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Directory/profile URIs.
    pub directories: BTreeMap<PropertyId, ContactProperty<ContactResource>>,
    /// Search/group keywords.
    pub keywords: BTreeSet<String>,
    /// Preferred time zone.
    pub time_zone: Option<String>,
    /// Card creation time.
    pub created: Option<UtcDateTime>,
    /// Card last-update time.
    pub updated: Option<UtcDateTime>,
    /// Per-object revision tokens.
    pub revisions: RevisionTokens,
    /// Normalized provider extensions.
    pub extended: ExtendedProperties,
    /// Raw vCard.
    pub raw_vcard: Option<RawVcard>,
    /// Raw JSContact.
    pub raw_jscontact: Option<RawJsContact>,
    /// Raw Graph/Google/provider JSON.
    pub raw_provider_json: Option<RawProviderJson>,
}

impl ContactCard {
    /// Creates a minimally valid card.
    #[must_use]
    pub fn new(id: ContactId, address_books: Memberships<AddressBookId>) -> Self {
        Self {
            id,
            address_books,
            ..Self::default()
        }
    }

    /// Returns the best name present on this source card.
    #[must_use]
    pub fn display_name(&self) -> Option<String> {
        self.name.as_ref().and_then(ContactName::display)
    }

    /// Returns which neutral fields are populated.
    #[must_use]
    pub fn populated_fields(&self) -> ContactFieldSet {
        ContactFieldSet::from_card(self)
    }
}

impl Default for ContactCard {
    fn default() -> Self {
        Self {
            id: ContactId::try_from("unknown").expect("static non-empty id"),
            uid: None,
            address_books: Memberships::of_one(
                AddressBookId::try_from("unknown").expect("static non-empty id"),
            ),
            source_class: ContactSourceClass::Personal,
            is_writable: false,
            kind: ContactKind::default(),
            name: None,
            nicknames: BTreeMap::new(),
            emails: BTreeMap::new(),
            phones: BTreeMap::new(),
            addresses: BTreeMap::new(),
            organizations: BTreeMap::new(),
            titles: BTreeMap::new(),
            anniversaries: BTreeMap::new(),
            notes: BTreeMap::new(),
            urls: BTreeMap::new(),
            media: BTreeMap::new(),
            online_services: BTreeMap::new(),
            relations: BTreeMap::new(),
            languages: BTreeMap::new(),
            members: BTreeMap::new(),
            personal_info: BTreeMap::new(),
            calendars: BTreeMap::new(),
            scheduling_addresses: BTreeMap::new(),
            crypto_keys: BTreeMap::new(),
            directories: BTreeMap::new(),
            keywords: BTreeSet::new(),
            time_zone: None,
            created: None,
            updated: None,
            revisions: RevisionTokens::none(),
            extended: ExtendedProperties::new(),
            raw_vcard: None,
            raw_jscontact: None,
            raw_provider_json: None,
        }
    }
}
