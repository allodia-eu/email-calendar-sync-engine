//! Google People contact, suggested-person, directory, and group sources.

use async_trait::async_trait;
use engine_core::{
    contact::{AddressBook, ContactCard, ContactDraft, ContactKind, ContactPatch},
    error::FailureClass,
    ids::{AccountId, ContactId},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactDestination, ContactPhoto, ContactSourceSync,
    ContactWriteReceipt, ContactsProvider, Provider, ProviderResult, ScopeSync, WriteGuard,
};
use serde_json::Value;

use crate::{
    contact_normalize,
    contact_source::{
        GoogleContactSource, contact_id, is_source_absent, person_fields, provider_key,
        require_owned, supported_fields,
    },
    contact_write,
    error::GoogleError,
    transport::{GoogleClient, encode_query_value},
};

/// The pixel size requested for a People photo.
///
/// Matches the size asked of the other providers, so a row is drawn from comparable
/// bytes whichever account the sender is in.
const PHOTO_SIZE: u16 = 240;

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

    fn page_url(&self, page_token: Option<&str>, sync_token: Option<&str>) -> String {
        let mut url = self.client.people_url(&self.source.path());
        if let Some(token) = sync_token {
            url = format!("{url}&syncToken={}", encode_query_value(token));
        }
        if let Some(token) = page_token {
            url = format!("{url}&pageToken={}", encode_query_value(token));
        }
        url
    }

    async fn source_sync(
        &self,
        cursor: Option<&SyncState>,
    ) -> Result<ContactSourceSync<ContactCard>, GoogleError> {
        // contactGroups.list has pagination but no incremental sync-token contract.
        // Its stable sentinel is only a store cursor: every pass remains a snapshot.
        let delta_cursor = self.source.is_incremental().then_some(cursor).flatten();
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
                    url = self.client.people_url(&self.source.path());
                    continue;
                }
                // An optional source may be refused by *shape* as well as by permission:
                // a consumer account has no Workspace directory, and
                // `people:listDirectoryPeople` answers `400 FAILED_PRECONDITION`
                // ("Must be a G Suite domain user"), never `403`. Both mean "this source
                // does not exist for this account", so both degrade rather than fail the
                // sync.
                //
                // The `400` arm keys on `FAILED_PRECONDITION` specifically, **not** on
                // the bare status: People also answers `400 INVALID_ARGUMENT` when a
                // request is simply wrong (an unsupported `readMask` field), and
                // degrading that would turn a permanent adapter bug into a silently
                // empty address book.
                Err(error) if !self.source.writable() && is_source_absent(&error) => {
                    return Ok(ContactSourceSync::Unavailable(
                        engine_provider::ContactUnavailable {
                            reason: "Google People source unavailable for this account".into(),
                        },
                    ));
                }
                Err(error) => return Err(error),
            };
            let entries = match page.get(self.source.page_key()).and_then(Value::as_array) {
                Some(entries) => entries.as_slice(),
                // A People page with nothing to report **omits the collection key
                // entirely** — a quiet incremental sync answers exactly
                // `{"nextSyncToken": "…"}`. That is the steady state, not a malformed
                // page, so it must read as "no entries" and advance the cursor. Only a
                // page that proves itself well-formed by carrying a cursor may be read
                // this way; one with neither collection nor token is malformed and must
                // not advance anything (a token-less source like `contactGroups` is
                // therefore still strict, so a bad page can never empty the store).
                None if self.source.is_incremental() && page.get("nextSyncToken").is_some() => &[],
                None => {
                    return Err(GoogleError::protocol(format!(
                        "Google contact page missing {}",
                        self.source.page_key()
                    )));
                }
            };
            for value in entries {
                if contact_normalize::deleted(value) {
                    if let Some(id) = value.get("resourceName").and_then(Value::as_str) {
                        removed.push(provider_key(id)?);
                    }
                } else if self.source == GoogleContactSource::Groups {
                    changed.push(contact_normalize::group_card(
                        value,
                        self.source.address_book(),
                    )?);
                } else {
                    changed.push(contact_normalize::person(
                        value,
                        self.source.address_book(),
                        self.source.source_class(),
                        self.source.writable(),
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
        self.source.writable().then(|| ContactDestination {
            address_book: self.source.address_book(),
            source_class: self.source.source_class(),
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
            .get(&self.client.people_url(&format!(
                "/v1/{}?personFields={}",
                contact.as_str(),
                person_fields()
            )))
            .await?;
        Ok(contact_normalize::person(
            &value,
            self.source.address_book(),
            self.source.source_class(),
            self.source.writable(),
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
                &self.client.people_url(&format!(
                    "/v1/people:createContact?personFields={}",
                    person_fields()
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
                &self.client.people_url(&format!(
                    "/v1/{}:updateContact?updatePersonFields={fields}&personFields={}",
                    base.id.as_str(),
                    person_fields()
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
                    .people_url(&format!("/v1/{}:deleteContact", base.id.as_str())),
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
    ) -> ProviderResult<Option<ContactPhoto>> {
        let bytes = match self.client.get_bytes(&sized_photo_url(&media.uri)).await {
            Ok(bytes) => bytes,
            // A People photo URL that no longer resolves is a person without a
            // picture, not a broken sync.
            Err(GoogleError::Status {
                status: 404 | 410, ..
            }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(ContactPhoto::new(
            bytes,
            media.media_type.clone(),
            media
                .fingerprint
                .clone()
                .unwrap_or_else(|| media.uri.clone()),
        )))
    }
}

/// Asks Google's photo CDN for an avatar-sized rendering.
///
/// The size is an **option suffix on the path**, `…=s240`, not a query parameter:
/// `?sz=` is accepted and silently ignored, answering 200 with the original bytes, so
/// a caller using it gets a plausible image and no signal that nothing happened.
/// `photos[].url` arrives already carrying a suffix (`=s100`, sometimes with flags
/// like `-c`), and it is replaced wholesale — a URL with none serves the full stored
/// image, measured at 512x512 and 65 KB against 17 KB for the sized rendering.
///
/// The URL keeps its original form on the card, so the cache key is unaffected.
fn sized_photo_url(uri: &str) -> String {
    if uri.is_empty() {
        return uri.to_owned();
    }
    let (path, tail) = uri
        .split_once(['?', '#'])
        .map_or((uri, ""), |(path, _)| (path, &uri[path.len()..]));
    // Photo ids are unpadded URL-safe base64, so a `=` in the last segment can only be
    // the option delimiter.
    let segment_start = path.rfind('/').map_or(0, |index| index + 1);
    let base = match path[segment_start..].rfind('=') {
        Some(index) => &path[..segment_start + index],
        None => path,
    };
    format!("{base}=s{PHOTO_SIZE}{tail}")
}
