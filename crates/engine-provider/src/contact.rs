//! Provider-neutral contact sync, write, destination, and photo contracts.

use core::fmt;

use async_trait::async_trait;
use engine_core::{
    contact::{
        AddressBook, ContactCard, ContactDraft, ContactFieldSet, ContactPatch, ContactResource,
        ContactSourceClass,
    },
    ids::{AccountId, AddressBookId, ContactId},
    sync::{JmapDataType, SyncObject, SyncScope, SyncState},
};

use crate::{Provider, ProviderError, ProviderResult, ScopeSync, WriteGuard};

/// One explicit contact-write destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactDestination {
    /// Address-book identity.
    pub address_book: AddressBookId,
    /// Authority class.
    pub source_class: ContactSourceClass,
    /// Whether writes are accepted.
    pub writable: bool,
    /// Lost-update guard strength; `None` when read-only.
    pub write_guard: Option<WriteGuard>,
    /// Exact neutral fields accepted on create/patch.
    pub supported_fields: ContactFieldSet,
}

/// A source that could not be read independently from other contact sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactUnavailable {
    /// Stable, user-facing explanation (normally missing permission).
    pub reason: String,
}

/// A successful source sync or an independently unavailable optional source.
#[derive(Debug, Clone, PartialEq)]
pub enum ContactSourceSync<T: SyncObject> {
    /// Changes and cursor are available.
    Available {
        /// Normalized source update.
        sync: ScopeSync<T>,
        /// Whether an expired/invalid cursor forced a full snapshot restart.
        cursor_recovered: bool,
    },
    /// This source is unavailable while sibling sources may continue.
    Unavailable(ContactUnavailable),
}

/// The identity a create/patch resolved to before canonical refetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactWriteReceipt {
    /// Provider-assigned card id.
    pub contact: ContactId,
}

impl ContactWriteReceipt {
    /// Creates a write receipt.
    #[must_use]
    pub fn new(contact: ContactId) -> Self {
        Self { contact }
    }
}

/// Authenticated photo bytes and cache-invalidation metadata.
#[derive(Clone, PartialEq, Eq)]
pub struct ContactPhoto {
    bytes: Box<[u8]>,
    /// Media type, when known.
    pub media_type: Option<String>,
    /// Provider revision/media fingerprint.
    pub fingerprint: String,
}

impl ContactPhoto {
    /// Creates a fetched photo.
    #[must_use]
    pub fn new(
        bytes: impl Into<Vec<u8>>,
        media_type: Option<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            bytes: bytes.into().into_boxed_slice(),
            media_type,
            fingerprint: fingerprint.into(),
        }
    }

    /// Returns photo bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the photo into its bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_vec()
    }
}

impl fmt::Debug for ContactPhoto {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContactPhoto")
            .field("len", &self.bytes.len())
            .field("media_type", &self.media_type)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Contact-capable provider behavior, separate from the mail/calendar
/// [`Provider`] surface so source-bound adapters implement only this domain.
#[async_trait]
pub trait ContactsProvider: Provider {
    /// Address-book/source discovery scope.
    fn address_book_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::AddressBook,
        }
    }

    /// Contact-card scope for this source-bound adapter.
    fn contact_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::ContactCard,
        }
    }

    /// Destination metadata for this source, when it can accept creates.
    fn contact_destination(&self) -> Option<ContactDestination> {
        None
    }

    /// Discovers address books/contact sources.
    async fn sync_address_books(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<AddressBook>> {
        let _ = (account, cursor);
        Err(ProviderError::invalid_state(
            "provider does not support contact sync",
        ))
    }

    /// Syncs cards for this source.
    async fn sync_contacts(
        &self,
        account: &AccountId,
        cursor: Option<&SyncState>,
    ) -> ProviderResult<ContactSourceSync<ContactCard>> {
        let _ = (account, cursor);
        Err(ProviderError::invalid_state(
            "provider does not support contact sync",
        ))
    }

    /// Fetches one server-canonical card without advancing the normal cursor.
    async fn fetch_contact(
        &self,
        account: &AccountId,
        contact: &ContactId,
    ) -> ProviderResult<ContactCard> {
        let _ = (account, contact);
        Err(ProviderError::invalid_state(
            "provider does not support direct contact fetch",
        ))
    }

    /// Creates a card in the draft's explicit address book.
    async fn create_contact(
        &self,
        account: &AccountId,
        draft: &ContactDraft,
    ) -> ProviderResult<ContactWriteReceipt> {
        let _ = (account, draft);
        Err(ProviderError::invalid_state(
            "provider does not support contact writes",
        ))
    }

    /// Applies targeted intent to one source card.
    async fn patch_contact(
        &self,
        account: &AccountId,
        base: &ContactCard,
        patch: &ContactPatch,
    ) -> ProviderResult<ContactWriteReceipt> {
        let _ = (account, base, patch);
        Err(ProviderError::invalid_state(
            "provider does not support contact writes",
        ))
    }

    /// Deletes one source card. Already absent is success.
    async fn delete_contact(&self, account: &AccountId, base: &ContactCard) -> ProviderResult<()> {
        let _ = (account, base);
        Err(ProviderError::invalid_state(
            "provider does not support contact writes",
        ))
    }

    /// Fetches an authenticated photo/media resource.
    async fn fetch_contact_photo(
        &self,
        account: &AccountId,
        card: &ContactCard,
        media: &ContactResource,
    ) -> ProviderResult<ContactPhoto> {
        let _ = (account, card, media);
        Err(ProviderError::invalid_state(
            "provider does not support contact photos",
        ))
    }
}

#[cfg(test)]
mod tests {
    use engine_core::{
        contact::{ContactField, ContactKind},
        membership::Memberships,
        sync::SyncUpdate,
    };

    use super::*;
    use crate::{Capabilities, ConnectionInfo};

    struct Unsupported;

    #[async_trait]
    impl Provider for Unsupported {
        fn connection_info(&self) -> ConnectionInfo {
            ConnectionInfo::new(Capabilities::none())
        }
    }

    #[async_trait]
    impl ContactsProvider for Unsupported {}

    fn account() -> AccountId {
        AccountId::try_from("account").unwrap()
    }

    fn card() -> ContactCard {
        ContactCard::new(
            ContactId::try_from("contact").unwrap(),
            Memberships::of_one(AddressBookId::try_from("book").unwrap()),
        )
    }

    #[test]
    fn contact_value_types_expose_their_data_without_photo_bytes_in_debug() {
        let destination = ContactDestination {
            address_book: AddressBookId::try_from("book").unwrap(),
            source_class: ContactSourceClass::Personal,
            writable: true,
            write_guard: Some(WriteGuard::Absent),
            supported_fields: ContactFieldSet::from_fields([
                ContactField::Kind,
                ContactField::Name,
            ]),
        };
        assert!(destination.supported_fields.contains(ContactField::Name));

        let receipt = ContactWriteReceipt::new(ContactId::try_from("contact").unwrap());
        assert_eq!(receipt.contact.as_str(), "contact");
        let photo = ContactPhoto::new(vec![1, 2, 3], Some("image/jpeg".into()), "revision");
        let debug = format!("{photo:?}");
        assert!(debug.contains("len: 3"));
        assert!(!debug.contains("[1, 2, 3]"));
        assert_eq!(photo.as_bytes(), &[1, 2, 3]);
        assert_eq!(photo.into_bytes(), vec![1, 2, 3]);

        let unavailable = ContactSourceSync::<ContactCard>::Unavailable(ContactUnavailable {
            reason: "missing permission".into(),
        });
        assert!(matches!(
            unavailable,
            ContactSourceSync::Unavailable(ContactUnavailable { reason })
                if reason == "missing permission"
        ));
        let available = ContactSourceSync::Available {
            sync: ScopeSync::new(
                SyncUpdate::snapshot(
                    vec![card()],
                    [engine_core::ids::ProviderKey::new("contact").unwrap()]
                        .into_iter()
                        .collect(),
                ),
                SyncState::new("next"),
            ),
            cursor_recovered: true,
        };
        assert!(matches!(
            available,
            ContactSourceSync::Available {
                cursor_recovered: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn default_contact_provider_surface_is_explicitly_unsupported() {
        let provider = Unsupported;
        let account = account();
        let card = card();
        let draft = ContactDraft {
            address_book: AddressBookId::try_from("book").unwrap(),
            card: card.clone(),
        };
        let patch = ContactPatch {
            kind: Some(engine_core::contact::FieldPatch::Set(
                ContactKind::Organization,
            )),
            ..ContactPatch::default()
        };
        let media = ContactResource {
            uri: "https://example.test/photo".into(),
            ..ContactResource::default()
        };

        assert!(matches!(
            provider.address_book_scope(&account),
            SyncScope::JmapType {
                data_type: JmapDataType::AddressBook,
                ..
            }
        ));
        assert!(matches!(
            provider.contact_scope(&account),
            SyncScope::JmapType {
                data_type: JmapDataType::ContactCard,
                ..
            }
        ));
        assert!(provider.contact_destination().is_none());
        assert!(provider.sync_address_books(&account, None).await.is_err());
        assert!(provider.sync_contacts(&account, None).await.is_err());
        assert!(provider.fetch_contact(&account, &card.id).await.is_err());
        assert!(provider.create_contact(&account, &draft).await.is_err());
        assert!(
            provider
                .patch_contact(&account, &card, &patch)
                .await
                .is_err()
        );
        assert!(provider.delete_contact(&account, &card).await.is_err());
        assert!(
            provider
                .fetch_contact_photo(&account, &card, &media)
                .await
                .is_err()
        );
    }
}
