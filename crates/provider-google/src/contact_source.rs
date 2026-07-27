//! Which Google People source an adapter instance is bound to, and everything that
//! follows from that choice: its address book, source class, writability, request path,
//! and field mask.
//!
//! Kept apart from the provider so the per-source *data* — notably each source's field
//! mask, which People validates strictly and differently per endpoint — reads as one
//! table rather than as branches scattered through the sync loop.

use engine_core::{
    contact::{ContactField, ContactFieldSet, ContactSourceClass},
    ids::{AddressBookId, ContactId, ProviderKey},
};
use engine_provider::ProviderResult;

use crate::error::GoogleError;

const CONNECTIONS: &str = "google-connections";
const OTHER: &str = "google-other-contacts";
const DIRECTORY: &str = "google-directory";
const GROUPS: &str = "google-contact-groups";

/// The `personFields` mask for owned connections and directory people.
const PERSON_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,birthdays,biographies,urls,relations,userDefined,photos,memberships,metadata";

/// The `readMask` `otherContacts.list` accepts — a strict subset of [`PERSON_FIELDS`].
///
/// Suggested contacts are derived from correspondence, so People exposes only the
/// fields it can infer and **rejects** the rest: asking for any of `nicknames`,
/// `addresses`, `organizations`, `birthdays`, `biographies`, `urls`, `relations`,
/// `userDefined`, or `memberships` fails the whole request with
/// `400 INVALID_ARGUMENT` ("Request field '…' not allowed for other contacts read
/// requests"). Determined by probing each field against the live API.
const OTHER_CONTACT_FIELDS: &str = "names,emailAddresses,phoneNumbers,photos,metadata";

/// Independently permissioned Google People source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoogleContactSource {
    /// User-owned connections.
    Connections,
    /// Suggested Other Contacts.
    OtherContacts,
    /// Workspace domain directory.
    Directory,
    /// Contact groups as group cards.
    Groups,
}

impl GoogleContactSource {
    /// The address book this source syncs into.
    pub(crate) fn address_book(self) -> AddressBookId {
        AddressBookId::try_from(match self {
            Self::Connections => CONNECTIONS,
            Self::OtherContacts => OTHER,
            Self::Directory => DIRECTORY,
            Self::Groups => GROUPS,
        })
        .expect("static id")
    }

    /// How a host should present cards from this source.
    pub(crate) fn source_class(self) -> ContactSourceClass {
        match self {
            Self::Connections | Self::Groups => ContactSourceClass::Personal,
            Self::OtherContacts => ContactSourceClass::Suggested,
            Self::Directory => ContactSourceClass::Directory,
        }
    }

    /// Only owned connections accept writes.
    pub(crate) fn writable(self) -> bool {
        self == Self::Connections
    }

    /// Whether this source has an incremental sync-token contract.
    ///
    /// `contactGroups.list` paginates but has no sync token, so every pass stays a
    /// snapshot and its cursor is only a store sentinel.
    pub(crate) fn is_incremental(self) -> bool {
        self != Self::Groups
    }

    /// The API-relative first-page path, including this source's field mask.
    pub(crate) fn path(self) -> String {
        match self {
            Self::Connections => format!(
                "/v1/people/me/connections?personFields={PERSON_FIELDS}&requestSyncToken=true&pageSize=1000"
            ),
            Self::OtherContacts => format!(
                "/v1/otherContacts?readMask={OTHER_CONTACT_FIELDS}&requestSyncToken=true&pageSize=1000"
            ),
            Self::Directory => format!(
                "/v1/people:listDirectoryPeople?readMask={PERSON_FIELDS}&sources=DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE&requestSyncToken=true&pageSize=1000"
            ),
            Self::Groups => {
                "/v1/contactGroups?pageSize=1000&groupFields=name,groupType,memberCount,metadata"
                    .into()
            }
        }
    }

    /// The JSON key holding this source's page entries.
    pub(crate) fn page_key(self) -> &'static str {
        match self {
            Self::Connections => "connections",
            Self::OtherContacts => "otherContacts",
            Self::Directory => "people",
            Self::Groups => "contactGroups",
        }
    }
}

/// The `personFields` mask a single-person read or write requests.
pub(crate) const fn person_fields() -> &'static str {
    PERSON_FIELDS
}

/// Neutral fields the writable source can round-trip.
pub(crate) fn supported_fields() -> ContactFieldSet {
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
        ContactField::Relations,
        ContactField::Keywords,
    ])
}

/// Rejects a write against a read-only source.
///
/// # Errors
///
/// Returns an invalid-state error unless `source` is [`GoogleContactSource::Connections`].
pub(crate) fn require_owned(source: GoogleContactSource) -> ProviderResult<()> {
    if source.writable() {
        Ok(())
    } else {
        Err(engine_provider::ProviderError::invalid_state(
            "Google contact source is read-only",
        ))
    }
}

/// Whether `error` means "this optional source does not exist for this account" rather
/// than "this request failed".
///
/// A permission-gated source answers `403`; a source that simply does not apply to the
/// account type answers `400 FAILED_PRECONDITION` (a consumer account has no Workspace
/// directory). A `400` with any other reason — notably `INVALID_ARGUMENT` for an
/// unsupported `readMask` — is a real defect and must surface.
pub(crate) fn is_source_absent(error: &GoogleError) -> bool {
    match error {
        GoogleError::Status { status: 403, .. } => true,
        GoogleError::Status {
            status: 400,
            reason,
            ..
        } => reason.as_deref() == Some("FAILED_PRECONDITION"),
        _ => false,
    }
}

pub(crate) fn provider_key(value: &str) -> Result<ProviderKey, GoogleError> {
    ProviderKey::new(value).map_err(|error| GoogleError::protocol(error.to_string()))
}

pub(crate) fn contact_id(value: &str) -> Result<ContactId, GoogleError> {
    ContactId::try_from(value).map_err(|error| GoogleError::protocol(error.to_string()))
}
