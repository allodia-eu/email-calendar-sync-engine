//! CardDAV address-book discovery, RFC 6578 sync, and conditional writes.

use std::collections::BTreeSet;

use async_trait::async_trait;
use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactPatch, ContactResource, ContactSourceClass,
    },
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId},
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    Capabilities, ConnectionInfo, ContactDestination, ContactPhoto, ContactSourceSync,
    ContactWriteReceipt, ContactsProvider, Provider, ProviderResult, ScopeSync, WriteGuard,
};
use engine_tls::TlsClientConfig;

use crate::{
    carddav_ops::{
        bind_collection, contact_id, contact_report, contact_update, decode_data_uri,
        discover_home, encode_segment, fallback_contact_sync, list_address_books, multiget_report,
        normalize_response, require_writable, stable_suffix, supported_fields,
    },
    error::CalDavError,
    transport::{Credentials, DavClient, DavExecutor, DavMethod, Precondition, WriteRequest},
    vcard_write,
};

const LIST_CURSOR: &str = "carddav-address-book-list";

/// What a CardDAV collection can do, gated on whether this principal may write to it.
fn capabilities(writable: bool) -> Capabilities {
    let capabilities = Capabilities::none()
        .with_contacts()
        .with_contact_groups()
        .with_contact_photos();
    if writable {
        capabilities.with_contact_writes(WriteGuard::Enforced)
    } else {
        capabilities
    }
}

/// CardDAV connection settings.
#[derive(Debug, Clone)]
pub struct CardDavConfig {
    /// Server origin.
    pub base_url: String,
    /// HTTP authentication.
    pub credentials: Credentials,
    /// Discovery path, normally `/.well-known/carddav`.
    pub discovery_path: String,
    /// Home-relative name or absolute href of the bound address book.
    pub address_book: String,
    /// Shared TLS trust policy.
    pub tls: TlsClientConfig,
}

impl CardDavConfig {
    /// Creates settings bound to the `default` address book.
    #[must_use]
    pub fn new(base_url: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            credentials,
            discovery_path: "/.well-known/carddav".into(),
            address_book: "default".into(),
            tls: TlsClientConfig::default(),
        }
    }

    /// Binds a different address book.
    #[must_use]
    pub fn with_address_book(mut self, address_book: impl Into<String>) -> Self {
        self.address_book = address_book.into();
        self
    }

    /// Overrides TLS trust.
    #[must_use]
    pub fn with_tls(mut self, tls: TlsClientConfig) -> Self {
        self.tls = tls;
        self
    }
}

/// CardDAV adapter bound to one address-book collection.
pub struct CardDavProvider {
    executor: Box<dyn DavExecutor>,
    home_href: String,
    collection: AddressBookId,
    /// Every address book the home listed as writable at connect time. Kept so
    /// [`CardDavProvider::rebind`] can answer "may I write here?" for the collection it
    /// switches to without repeating discovery — the alternative, assuming the worst,
    /// silently turned a rebound provider read-only.
    writable_books: BTreeSet<AddressBookId>,
    writable: bool,
    capabilities: Capabilities,
}

impl core::fmt::Debug for CardDavProvider {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CardDavProvider")
            .field("home_href", &self.home_href)
            .field("collection", &self.collection)
            .field("writable", &self.writable)
            .finish_non_exhaustive()
    }
}

impl CardDavProvider {
    /// Connects, discovers the address-book home, and binds one collection.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] for transport, discovery, or malformed collection data.
    pub async fn connect(config: CardDavConfig) -> Result<Self, CalDavError> {
        let client = DavClient::new(&config.base_url, config.credentials, &config.tls)?;
        Self::with_executor(
            Box::new(client),
            &config.discovery_path,
            &config.address_book,
        )
        .await
    }

    pub(crate) async fn with_executor(
        executor: Box<dyn DavExecutor>,
        discovery_path: &str,
        address_book: &str,
    ) -> Result<Self, CalDavError> {
        let home_href = discover_home(executor.as_ref(), discovery_path).await?;
        let collection = bind_collection(&home_href, address_book)?;
        let writable_books: BTreeSet<AddressBookId> =
            list_address_books(executor.as_ref(), &home_href)
                .await?
                .into_iter()
                .filter(|book| book.is_writable)
                .map(|book| book.id)
                .collect();
        let writable = writable_books.contains(&collection);
        Ok(Self {
            executor,
            home_href,
            capabilities: capabilities(writable),
            collection,
            writable_books,
            writable,
        })
    }

    /// Rebinds to another address book in the same home, without repeating discovery.
    ///
    /// Write capability is re-derived for the new collection from the privileges the
    /// home reported at connect time, so a rebind onto a writable book stays writable
    /// and one onto a read-only book stops advertising a destination.
    ///
    /// # Errors
    ///
    /// Returns [`CalDavError`] when `address_book` cannot form a valid address-book id.
    pub fn rebind(self, address_book: &str) -> Result<Self, CalDavError> {
        let collection = bind_collection(&self.home_href, address_book)?;
        let writable = self.writable_books.contains(&collection);
        Ok(Self {
            collection,
            writable,
            capabilities: capabilities(writable),
            ..self
        })
    }

    fn href(&self) -> &str {
        self.collection.as_str()
    }
}

#[async_trait]
impl Provider for CardDavProvider {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo {
            http_version: self.executor.http_version(),
            ..ConnectionInfo::new(self.capabilities)
        }
    }
}

#[async_trait]
impl ContactsProvider for CardDavProvider {
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::CardDavAddressBookList {
            account: account.clone(),
        }
    }

    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::CardDavAddressBook {
            account: account.clone(),
            address_book: self.collection.clone(),
        }
    }

    fn contact_destination(&self) -> Option<ContactDestination> {
        self.writable.then(|| ContactDestination {
            address_book: self.collection.clone(),
            source_class: ContactSourceClass::Personal,
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
        let books = list_address_books(self.executor.as_ref(), &self.home_href).await?;
        let present = books.iter().map(|book| book.id.key().clone()).collect();
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(books, present),
                SyncState::new(LIST_CURSOR),
            ),
            cursor_recovered: false,
        })
    }

    async fn sync_contacts(
        &self,
        _account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        if cursor.is_some_and(|cursor| cursor.as_str().starts_with("ctag:")) {
            let sync = fallback_contact_sync(
                self.executor.as_ref(),
                self.href(),
                &self.collection,
                self.writable,
                cursor,
            )
            .await?;
            return Ok(ContactSourceSync::Available {
                sync,
                cursor_recovered: false,
            });
        }
        let token = cursor.map_or("", SyncState::as_str);
        let mut recovered = false;
        let mut snapshot = token.is_empty();
        let report = match contact_report(self.executor.as_ref(), self.href(), token).await {
            Ok(report) => report,
            Err(CalDavError::Status {
                status: 405 | 501, ..
            }) => {
                let sync = fallback_contact_sync(
                    self.executor.as_ref(),
                    self.href(),
                    &self.collection,
                    self.writable,
                    cursor,
                )
                .await?;
                return Ok(ContactSourceSync::Available {
                    sync,
                    cursor_recovered: cursor.is_some(),
                });
            }
            Err(error) if !snapshot && error.failure_class() == FailureClass::NeedsResync => {
                recovered = true;
                snapshot = true;
                contact_report(self.executor.as_ref(), self.href(), "").await?
            }
            Err(error) => return Err(error.into()),
        };
        let next = report
            .sync_token
            .as_deref()
            .map(SyncState::new)
            .ok_or_else(|| CalDavError::protocol("CardDAV sync response had no sync-token"))?;
        let update = contact_update(&report, &self.collection, self.writable, snapshot);
        Ok(ContactSourceSync::Available {
            sync: ScopeSync::new(update, next),
            cursor_recovered: recovered,
        })
    }

    async fn fetch_contact(
        &self,
        _account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        let body = multiget_report(contact.as_str());
        let report = self
            .executor
            .send(DavMethod::Report, self.href(), "1", body)
            .await?
            .into_multistatus()?;
        report
            .responses
            .iter()
            .find_map(|response| normalize_response(response, &self.collection, self.writable))
            .ok_or_else(|| CalDavError::protocol("addressbook-multiget returned no card").into())
    }

    async fn create_contact(
        &self,
        _account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        require_writable(self.writable)?;
        if draft.address_book != self.collection {
            return Err(engine_provider::ProviderError::invalid_state(
                "CardDAV draft targets a different address book",
            ));
        }
        let suffix = draft
            .card
            .uid
            .as_deref()
            .map_or_else(|| stable_suffix(&draft.card), str::to_owned);
        let href = format!("{}{}.vcf", self.href(), encode_segment(&suffix));
        self.executor
            .send_write(WriteRequest {
                method: DavMethod::Put,
                href: href.clone(),
                content_type: Some("text/vcard; charset=utf-8"),
                precondition: Precondition::IfNoneMatch,
                body: vcard_write::build_vcard(&draft.card),
            })
            .await?
            .into_write_etag()?;
        Ok(ContactWriteReceipt::new(contact_id(&href)?))
    }

    async fn patch_contact(
        &self,
        _account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        require_writable(self.writable)?;
        let etag = base
            .revisions
            .etag
            .as_ref()
            .ok_or_else(|| CalDavError::protocol("CardDAV patch requires ETag"))?;
        self.executor
            .send_write(WriteRequest {
                method: DavMethod::Put,
                href: base.id.as_str().to_owned(),
                content_type: Some("text/vcard; charset=utf-8"),
                precondition: Precondition::IfMatch(etag.as_str().to_owned()),
                body: vcard_write::patch_vcard(base, patch)?,
            })
            .await?
            .into_write_etag()?;
        Ok(ContactWriteReceipt::new(base.id.clone()))
    }

    async fn delete_contact(&self, _account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        require_writable(self.writable)?;
        let etag = base
            .revisions
            .etag
            .as_ref()
            .ok_or_else(|| CalDavError::protocol("CardDAV delete requires ETag"))?;
        let result = self
            .executor
            .send_write(WriteRequest {
                method: DavMethod::Delete,
                href: base.id.as_str().to_owned(),
                content_type: None,
                precondition: Precondition::IfMatch(etag.as_str().to_owned()),
                body: String::new(),
            })
            .await;
        match result {
            Ok(response) if (200..300).contains(&response.status) => Ok(()),
            Ok(response) if matches!(response.status, 404 | 410) => Ok(()),
            Ok(response) => Err(CalDavError::status(response.status, response.body).into()),
            Err(error) => Err(error.into()),
        }
    }

    async fn fetch_contact_photo(
        &self,
        _account: &AccountId,
        _card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<ContactPhoto> {
        let bytes = if let Some(data) = media.uri.strip_prefix("data:") {
            decode_data_uri(data)?
        } else {
            self.executor.get_bytes(&media.uri).await?
        };
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
