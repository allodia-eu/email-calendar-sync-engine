//! Microsoft Graph personal, organizational, and directory contact provider.

use std::collections::VecDeque;

use async_trait::async_trait;
use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactField, ContactFieldSet, ContactPatch,
        ContactResource, ContactSourceClass,
    },
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId, ProviderKey},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactDestination, ContactPhoto, ContactSourceSync,
    ContactUnavailable, ContactWriteReceipt, ContactsProvider, Provider, ProviderResult, ScopeSync,
    WriteGuard,
};
use serde_json::Value;

use crate::{
    contact_normalize, contact_photo, contact_write, error::GraphError, transport::GraphClient,
};

const ROOT_BOOK: &str = "graph-personal-root";
const ORG_BOOK: &str = "graph-organizational-contacts";
const DIRECTORY_BOOK: &str = "graph-directory-users";
const FOLDER_CURSOR: &str = "graph-contact-folders";
const CONTACT_SELECT: &str = "id,changeKey,displayName,givenName,middleName,surname,title,generation,emailAddresses,businessPhones,homePhones,mobilePhone,businessAddress,homeAddress,otherAddress,companyName,department,jobTitle,personalNotes,birthday,businessHomePage,categories";
const USER_SELECT: &str = "id,displayName,givenName,surname,mail,userPrincipalName,proxyAddresses,businessPhones,mobilePhone,officeLocation,companyName,department,jobTitle,userType";

/// Which Graph contact source one adapter instance syncs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphContactSource {
    /// Personal root contacts or one discovered personal folder.
    Personal(AddressBookId),
    /// Tenant organizational contacts.
    Organizational,
    /// Tenant directory users.
    Directory,
}

/// A Graph contact provider bound to one source.
pub struct GraphContactProvider {
    client: GraphClient,
    source: GraphContactSource,
    capabilities: Capabilities,
}

impl core::fmt::Debug for GraphContactProvider {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("GraphContactProvider")
            .field("source", &self.source)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl GraphContactProvider {
    /// Binds to personal root contacts.
    #[must_use]
    pub fn personal(client: GraphClient) -> Self {
        Self::new(
            client,
            GraphContactSource::Personal(AddressBookId::try_from(ROOT_BOOK).expect("static id")),
        )
    }

    /// Binds to one discovered personal contact folder.
    #[must_use]
    pub fn personal_folder(client: GraphClient, folder: AddressBookId) -> Self {
        Self::new(client, GraphContactSource::Personal(folder))
    }

    /// Binds to organizational contacts.
    #[must_use]
    pub fn organizational(client: GraphClient) -> Self {
        Self::new(client, GraphContactSource::Organizational)
    }

    /// Binds to directory users.
    #[must_use]
    pub fn directory(client: GraphClient) -> Self {
        Self::new(client, GraphContactSource::Directory)
    }

    fn new(client: GraphClient, source: GraphContactSource) -> Self {
        let mut capabilities = Capabilities::none().with_contacts().with_contact_photos();
        if matches!(source, GraphContactSource::Personal(_)) {
            capabilities = capabilities.with_contact_writes(WriteGuard::Absent);
        }
        Self {
            client,
            source,
            capabilities,
        }
    }

    fn address_book(&self) -> AddressBookId {
        match &self.source {
            GraphContactSource::Personal(book) => book.clone(),
            GraphContactSource::Organizational => {
                AddressBookId::try_from(ORG_BOOK).expect("static id")
            }
            GraphContactSource::Directory => {
                AddressBookId::try_from(DIRECTORY_BOOK).expect("static id")
            }
        }
    }

    fn source_class(&self) -> ContactSourceClass {
        match self.source {
            GraphContactSource::Personal(_) => ContactSourceClass::Personal,
            GraphContactSource::Organizational | GraphContactSource::Directory => {
                ContactSourceClass::Directory
            }
        }
    }

    fn writable(&self) -> bool {
        matches!(self.source, GraphContactSource::Personal(_))
    }

    fn contact_collection_path(&self) -> String {
        match &self.source {
            GraphContactSource::Personal(book) if book.as_str() == ROOT_BOOK => "/contacts".into(),
            GraphContactSource::Personal(book) => {
                format!("/contactFolders/{}/contacts", book.as_str())
            }
            GraphContactSource::Organizational => "/contacts".into(),
            GraphContactSource::Directory => "/users".into(),
        }
    }

    fn initial_delta_url(&self) -> String {
        let path = self.contact_collection_path();
        match self.source {
            GraphContactSource::Personal(_) => self
                .client
                .url(&format!("{path}/delta?$select={CONTACT_SELECT}")),
            GraphContactSource::Organizational => self
                .client
                .global_url(&format!("{path}/delta?$select={CONTACT_SELECT}")),
            GraphContactSource::Directory => self
                .client
                .global_url(&format!("{path}/delta?$select={USER_SELECT}")),
        }
    }

    async fn contact_sync(
        &self,
        cursor: Option<&SyncState>,
    ) -> Result<ContactSourceSync<ContactCard>, GraphError> {
        let mut recovered = false;
        let mut delta = cursor.is_some();
        let mut url = cursor.map_or_else(
            || self.initial_delta_url(),
            |cursor| cursor.as_str().to_owned(),
        );
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        loop {
            let page = match self.client.get(&url).await {
                Ok(page) => page,
                Err(error)
                    if cursor.is_some()
                        && changed.is_empty()
                        && error.failure_class() == FailureClass::NeedsResync =>
                {
                    recovered = true;
                    delta = false;
                    url = self.initial_delta_url();
                    continue;
                }
                Err(error) if self.is_optional_permission_error(&error) => {
                    return Ok(ContactSourceSync::Unavailable(ContactUnavailable {
                        reason: "Microsoft Graph contact source permission unavailable".into(),
                    }));
                }
                Err(error) => return Err(error),
            };
            for value in values(&page)? {
                if value.get("@removed").is_some() {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        removed.push(
                            ProviderKey::new(id)
                                .map_err(|error| GraphError::protocol(error.to_string()))?,
                        );
                    }
                } else {
                    changed.push(contact_normalize::card(
                        value,
                        self.address_book(),
                        self.source_class(),
                        self.writable(),
                    )?);
                }
            }
            if let Some(next) = page.get("@odata.nextLink").and_then(Value::as_str) {
                url = next.into();
                continue;
            }
            let cursor = page
                .get("@odata.deltaLink")
                .and_then(Value::as_str)
                .ok_or_else(|| GraphError::protocol("contact delta missing deltaLink"))?;
            let update = if delta {
                SyncUpdate::delta(changed, removed)
            } else {
                let present = changed.iter().map(|card| card.id.key().clone()).collect();
                SyncUpdate::snapshot(changed, present)
            };
            return Ok(ContactSourceSync::Available {
                sync: ScopeSync::new(update, SyncState::new(cursor)),
                cursor_recovered: recovered,
            });
        }
    }

    /// Whether `error` means "this optional source does not exist for this account"
    /// rather than "this sync failed".
    ///
    /// The tenant sources are gated on tenant-only permissions, so the obvious signal is
    /// `403`. A personal Microsoft account refuses them by *shape* instead: `/contacts`
    /// answers `400 BadRequest` ("This API is not supported for MSA accounts") and
    /// `/users` answers `401` with an empty message (see the captured fixtures under
    /// `tests/fixtures/error/`). Keying on `403` alone therefore fails a personal
    /// account's contact sync outright.
    ///
    /// Swallowing `401` here cannot hide a genuinely expired token: the same credential
    /// drives the personal source, which is never optional, so real authentication
    /// failures still surface from there.
    fn is_optional_permission_error(&self, error: &GraphError) -> bool {
        !matches!(self.source, GraphContactSource::Personal(_))
            && matches!(
                error,
                GraphError::Status {
                    status: 400 | 401 | 403,
                    ..
                }
            )
    }

    fn item_url(&self, contact: &ContactId) -> String {
        let path = format!("{}/{}", self.contact_collection_path(), contact.as_str());
        match self.source {
            GraphContactSource::Personal(_) => self.client.url(&path),
            GraphContactSource::Organizational | GraphContactSource::Directory => {
                self.client.global_url(&path)
            }
        }
    }
}

#[async_trait]
impl Provider for GraphContactProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.client.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }
}

#[async_trait]
impl ContactsProvider for GraphContactProvider {
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::GraphContactFolderList {
            account: account.clone(),
        }
    }

    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        match &self.source {
            GraphContactSource::Personal(address_book) => SyncScope::GraphContacts {
                account: account.clone(),
                address_book: address_book.clone(),
            },
            GraphContactSource::Organizational => SyncScope::GraphOrgContacts {
                account: account.clone(),
            },
            GraphContactSource::Directory => SyncScope::GraphDirectoryUsers {
                account: account.clone(),
            },
        }
    }

    fn contact_destination(&self) -> Option<ContactDestination> {
        self.writable().then(|| ContactDestination {
            address_book: self.address_book(),
            source_class: self.source_class(),
            writable: true,
            write_guard: Some(WriteGuard::Absent),
            supported_fields: supported_fields(),
        })
    }

    async fn sync_address_books(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let books = discover_folders(&self.client).await?;
        let present = books.iter().map(|book| book.id.key().clone()).collect();
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(books, present),
                SyncState::new(FOLDER_CURSOR),
            ),
            cursor_recovered: false,
        })
    }

    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        Ok(self.contact_sync(cursor).await?)
    }

    async fn fetch_contact(
        &self,
        _account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        let value = self.client.get(&self.item_url(contact)).await?;
        Ok(contact_normalize::card(
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
        require_personal(&self.source)?;
        if draft.card.kind != engine_core::contact::ContactKind::Individual {
            return Err(GraphError::protocol(
                "Microsoft Graph personal contacts support only individual cards",
            )
            .into());
        }
        let url = self.client.url(&self.contact_collection_path());
        let value = self
            .client
            .post(
                &url,
                "application/json",
                contact_write::create_body(&draft.card)?,
            )
            .await?
            .ok_or_else(|| GraphError::protocol("contact create returned no object"))?;
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| GraphError::protocol("contact create missing id"))?;
        Ok(ContactWriteReceipt::new(ContactId::try_from(id).map_err(
            |error| GraphError::protocol(error.to_string()),
        )?))
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        require_personal(&self.source)?;
        self.client
            .patch(
                &self.item_url(&base.id),
                "application/json",
                None,
                contact_write::patch_body(patch)?,
            )
            .await?;
        Ok(ContactWriteReceipt::new(base.id.clone()))
    }

    async fn delete_contact(&self, _account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        require_personal(&self.source)?;
        match self.client.delete(&self.item_url(&base.id), None).await {
            Ok(()) | Err(GraphError::Status { status: 404, .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn fetch_contact_photo(
        &self,
        _account: &AccountId,
        card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<Option<ContactPhoto>> {
        // Only a `user` offers sized photos; a `contact` has the singular resource.
        let sized = matches!(self.source, GraphContactSource::Directory);
        contact_photo::fetch(&self.client, &self.item_url(&card.id), sized, card, media).await
    }
}

async fn discover_folders(client: &GraphClient) -> Result<Vec<AddressBook>, GraphError> {
    let root_id = AddressBookId::try_from(ROOT_BOOK).expect("static id");
    let mut root = AddressBook::new(root_id, "Contacts", ContactSourceClass::Personal);
    root.is_writable = true;
    let mut books = vec![root];
    let mut queue = VecDeque::from([
        client.url("/contactFolders?$select=id,displayName,parentFolderId&$top=100")
    ]);
    while let Some(mut url) = queue.pop_front() {
        loop {
            let page = client.get(&url).await?;
            for value in values(&page)? {
                let book = contact_normalize::folder(value)?;
                queue.push_back(client.url(&format!(
                    "/contactFolders/{}/childFolders?$select=id,displayName,parentFolderId&$top=100",
                    book.id.as_str()
                )));
                books.push(book);
            }
            let Some(next) = page.get("@odata.nextLink").and_then(Value::as_str) else {
                break;
            };
            url = next.into();
        }
    }
    books.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(books)
}

fn values(page: &Value) -> Result<&Vec<Value>, GraphError> {
    page.get("value")
        .and_then(Value::as_array)
        .ok_or_else(|| GraphError::protocol("contact response missing value array"))
}

fn supported_fields() -> ContactFieldSet {
    ContactFieldSet::from_fields([
        ContactField::Kind,
        ContactField::Name,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Addresses,
        ContactField::Organizations,
        ContactField::Titles,
        ContactField::Notes,
        ContactField::Keywords,
    ])
}

fn require_personal(source: &GraphContactSource) -> ProviderResult<()> {
    if matches!(source, GraphContactSource::Personal(_)) {
        Ok(())
    } else {
        Err(engine_provider::ProviderError::invalid_state(
            "Graph directory contact source is read-only",
        ))
    }
}
