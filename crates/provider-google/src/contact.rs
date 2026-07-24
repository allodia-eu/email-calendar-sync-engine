//! Google People contact, suggested-person, directory, and group sources.

use async_trait::async_trait;
use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactField, ContactFieldSet, ContactKind,
        ContactPatch, ContactSourceClass,
    },
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId, ProviderKey},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactDestination, ContactPhoto, ContactSourceSync,
    ContactWriteReceipt, ContactsProvider, Provider, ProviderResult, ScopeSync, WriteGuard,
};
use serde_json::Value;

use crate::{contact_normalize, contact_write, error::GoogleError, transport::GoogleClient};

const CONNECTIONS: &str = "google-connections";
const OTHER: &str = "google-other-contacts";
const DIRECTORY: &str = "google-directory";
const GROUPS: &str = "google-contact-groups";
const PERSON_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,addresses,organizations,birthdays,biographies,urls,relations,userDefined,photos,memberships,metadata";

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

/// Google People adapter bound to one source.
pub struct GoogleContactProvider {
    client: GoogleClient,
    source: GoogleContactSource,
    capabilities: Capabilities,
}

impl core::fmt::Debug for GoogleContactProvider {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GoogleContactProvider")
            .field("source", &self.source)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl GoogleContactProvider {
    /// Binds to owned connections.
    #[must_use]
    pub fn connections(client: GoogleClient) -> Self {
        Self::new(client, GoogleContactSource::Connections)
    }

    /// Binds to Other Contacts.
    #[must_use]
    pub fn other_contacts(client: GoogleClient) -> Self {
        Self::new(client, GoogleContactSource::OtherContacts)
    }

    /// Binds to the Workspace directory.
    #[must_use]
    pub fn directory(client: GoogleClient) -> Self {
        Self::new(client, GoogleContactSource::Directory)
    }

    /// Binds to contact groups.
    #[must_use]
    pub fn groups(client: GoogleClient) -> Self {
        Self::new(client, GoogleContactSource::Groups)
    }

    fn new(client: GoogleClient, source: GoogleContactSource) -> Self {
        let mut capabilities = Capabilities::none().with_contacts().with_contact_photos();
        if source == GoogleContactSource::Connections {
            capabilities = capabilities.with_contact_writes(WriteGuard::Enforced);
        }
        if source == GoogleContactSource::Groups {
            capabilities = capabilities.with_contact_groups();
        }
        Self {
            client,
            source,
            capabilities,
        }
    }

    fn address_book(&self) -> AddressBookId {
        AddressBookId::try_from(match self.source {
            GoogleContactSource::Connections => CONNECTIONS,
            GoogleContactSource::OtherContacts => OTHER,
            GoogleContactSource::Directory => DIRECTORY,
            GoogleContactSource::Groups => GROUPS,
        })
        .expect("static id")
    }

    fn source_class(&self) -> ContactSourceClass {
        match self.source {
            GoogleContactSource::Connections | GoogleContactSource::Groups => {
                ContactSourceClass::Personal
            }
            GoogleContactSource::OtherContacts => ContactSourceClass::Suggested,
            GoogleContactSource::Directory => ContactSourceClass::Directory,
        }
    }

    fn writable(&self) -> bool {
        self.source == GoogleContactSource::Connections
    }

    fn initial_url(&self) -> String {
        let path = match self.source {
            GoogleContactSource::Connections => format!(
                "/v1/people/me/connections?personFields={PERSON_FIELDS}&requestSyncToken=true&pageSize=1000"
            ),
            GoogleContactSource::OtherContacts => format!(
                "/v1/otherContacts?readMask={PERSON_FIELDS}&requestSyncToken=true&pageSize=1000"
            ),
            GoogleContactSource::Directory => format!(
                "/v1/people:listDirectoryPeople?readMask={PERSON_FIELDS}&sources=DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE&requestSyncToken=true&pageSize=1000"
            ),
            GoogleContactSource::Groups => {
                "/v1/contactGroups?pageSize=1000&groupFields=name,groupType,memberCount,metadata"
                    .into()
            }
        };
        self.client.url(&path)
    }

    fn page_key(&self) -> &'static str {
        match self.source {
            GoogleContactSource::Connections => "connections",
            GoogleContactSource::OtherContacts => "otherContacts",
            GoogleContactSource::Directory => "people",
            GoogleContactSource::Groups => "contactGroups",
        }
    }

    fn page_url(&self, page_token: Option<&str>, sync_token: Option<&str>) -> String {
        let mut url = self.initial_url();
        if let Some(token) = sync_token {
            url = format!("{url}&syncToken={token}");
        }
        if let Some(token) = page_token {
            url = format!("{url}&pageToken={token}");
        }
        url
    }

    async fn source_sync(
        &self,
        cursor: Option<&SyncState>,
    ) -> Result<ContactSourceSync<ContactCard>, GoogleError> {
        // contactGroups.list has pagination but no incremental sync-token contract.
        // Its stable sentinel is only a store cursor: every pass remains a snapshot.
        let delta_cursor = (self.source != GoogleContactSource::Groups)
            .then_some(cursor)
            .flatten();
        let mut cursor_recovered = false;
        let mut is_delta = delta_cursor.is_some();
        let mut active_sync_token = delta_cursor.map(SyncState::as_str);
        let mut url = self.page_url(None, active_sync_token);
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        loop {
            let page = match self.client.get(&url).await {
                Ok(page) => page,
                Err(error)
                    if delta_cursor.is_some()
                        && changed.is_empty()
                        && error.failure_class() == FailureClass::NeedsResync =>
                {
                    cursor_recovered = true;
                    is_delta = false;
                    active_sync_token = None;
                    url = self.initial_url();
                    continue;
                }
                Err(GoogleError::Status { status: 403, .. }) if !self.writable() => {
                    return Ok(ContactSourceSync::Unavailable(
                        engine_provider::ContactUnavailable {
                            reason: "Google People source permission unavailable".into(),
                        },
                    ));
                }
                Err(error) => return Err(error),
            };
            let entries = page
                .get(self.page_key())
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    GoogleError::protocol(format!(
                        "Google contact page missing {}",
                        self.page_key()
                    ))
                })?;
            for value in entries {
                if contact_normalize::deleted(value) {
                    if let Some(id) = value.get("resourceName").and_then(Value::as_str) {
                        removed.push(provider_key(id)?);
                    }
                } else if self.source == GoogleContactSource::Groups {
                    changed.push(contact_normalize::group_card(value, self.address_book())?);
                } else {
                    changed.push(contact_normalize::person(
                        value,
                        self.address_book(),
                        self.source_class(),
                        self.writable(),
                    )?);
                }
            }
            if let Some(next) = page.get("nextPageToken").and_then(Value::as_str) {
                url = self.page_url(Some(next), active_sync_token);
                continue;
            }
            let next = if self.source == GoogleContactSource::Groups {
                "google-groups-snapshot"
            } else {
                page.get("nextSyncToken")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        GoogleError::protocol("Google contact page missing nextSyncToken")
                    })?
            };
            let update = if is_delta {
                SyncUpdate::delta(changed, removed)
            } else {
                let present = changed.iter().map(|card| card.id.key().clone()).collect();
                SyncUpdate::snapshot(changed, present)
            };
            return Ok(ContactSourceSync::Available {
                sync: ScopeSync::new(update, SyncState::new(next)),
                cursor_recovered,
            });
        }
    }
}

#[async_trait]
impl Provider for GoogleContactProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }
}

#[async_trait]
impl ContactsProvider for GoogleContactProvider {
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GoogleContactSourceList {
            account: account.clone(),
        }
    }

    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        match self.source {
            GoogleContactSource::Connections => SyncScope::GoogleContacts {
                account: account.clone(),
            },
            GoogleContactSource::OtherContacts => SyncScope::GoogleOtherContacts {
                account: account.clone(),
            },
            GoogleContactSource::Directory => SyncScope::GoogleDirectoryPeople {
                account: account.clone(),
            },
            GoogleContactSource::Groups => SyncScope::GoogleContactGroups {
                account: account.clone(),
            },
        }
    }

    fn contact_destination(&self) -> Option<ContactDestination> {
        self.writable().then(|| ContactDestination {
            address_book: self.address_book(),
            source_class: self.source_class(),
            writable: true,
            write_guard: Some(WriteGuard::Enforced),
            supported_fields: supported_fields(),
        })
    }

    async fn sync_address_books(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let books = contact_normalize::source_books();
        let present = books.iter().map(|book| book.id.key().clone()).collect();
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(books, present),
                SyncState::new("google-contact-sources"),
            ),
            cursor_recovered: false,
        })
    }

    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        Ok(self.source_sync(cursor).await?)
    }

    async fn fetch_contact(
        &self,
        _account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        let value = self
            .client
            .get(&self.client.url(&format!(
                "/v1/{}?personFields={PERSON_FIELDS}",
                contact.as_str()
            )))
            .await?;
        Ok(contact_normalize::person(
            &value,
            self.address_book(),
            self.source_class(),
            self.writable(),
        )?)
    }

    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        require_owned(self.source)?;
        if draft.card.kind != ContactKind::Individual {
            return Err(GoogleError::protocol(
                "Google People owned contacts support only individual cards",
            )
            .into());
        }
        let value = self
            .client
            .post(
                &self.client.url(&format!(
                    "/v1/people:createContact?personFields={PERSON_FIELDS}"
                )),
                "application/json",
                contact_write::create_body(draft)?,
            )
            .await?
            .ok_or_else(|| GoogleError::protocol("createContact returned no person"))?;
        Ok(ContactWriteReceipt::new(contact_id(
            value
                .get("resourceName")
                .and_then(Value::as_str)
                .ok_or_else(|| GoogleError::protocol("created person missing resourceName"))?,
        )?))
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        require_owned(self.source)?;
        let fields = contact_write::update_fields(patch)?;
        if fields.is_empty() {
            return Ok(ContactWriteReceipt::new(base.id.clone()));
        }
        self.client
            .patch(
                &self.client.url(&format!(
                    "/v1/{}:updateContact?updatePersonFields={fields}&personFields={PERSON_FIELDS}",
                    base.id.as_str()
                )),
                "application/json",
                base.revisions
                    .etag
                    .as_ref()
                    .map(engine_core::version::ETag::as_str),
                contact_write::patch_body(base, patch)?,
            )
            .await?;
        Ok(ContactWriteReceipt::new(base.id.clone()))
    }

    async fn delete_contact(&self, _account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        require_owned(self.source)?;
        match self
            .client
            .delete(
                &self
                    .client
                    .url(&format!("/v1/{}:deleteContact", base.id.as_str())),
                base.revisions
                    .etag
                    .as_ref()
                    .map(engine_core::version::ETag::as_str),
            )
            .await
        {
            Ok(()) | Err(GoogleError::Status { status: 404, .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn fetch_contact_photo(
        &self,
        _account: &AccountId,
        _card: &ContactCard,
        media: &engine_core::contact::ContactResource,
    ) -> ProviderResult<ContactPhoto> {
        let bytes = self.client.get_bytes(&media.uri).await?;
        Ok(ContactPhoto::new(
            bytes,
            media.media_type.clone(),
            media
                .fingerprint
                .clone()
                .unwrap_or_else(|| media.uri.clone()),
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
        ContactField::Relations,
        ContactField::Keywords,
    ])
}

fn require_owned(source: GoogleContactSource) -> ProviderResult<()> {
    if source == GoogleContactSource::Connections {
        Ok(())
    } else {
        Err(engine_provider::ProviderError::invalid_state(
            "Google contact source is read-only",
        ))
    }
}

fn provider_key(value: &str) -> Result<ProviderKey, GoogleError> {
    ProviderKey::new(value).map_err(|error| GoogleError::protocol(error.to_string()))
}

fn contact_id(value: &str) -> Result<ContactId, GoogleError> {
    ContactId::try_from(value).map_err(|error| GoogleError::protocol(error.to_string()))
}
