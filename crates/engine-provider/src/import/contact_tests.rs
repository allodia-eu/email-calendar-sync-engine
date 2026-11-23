use std::{collections::BTreeSet, num::NonZeroU64, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    contact::{AddressBook, ContactSourceClass, VcardVersion},
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId},
    raw::RawVcard,
    sync::SyncState,
    version::{ETag, RevisionTokens},
};

use super::{ContactImport, ContactImportLimits, ContactImporter, ImportedContact};
use crate::{AddressBookCollections, NewAddressBook, ProviderResult};

fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

fn book() -> AddressBookId {
    AddressBookId::try_from("/card/u/friends/").unwrap()
}

fn card(version: &str, padding: usize) -> ContactImport {
    ContactImport::new(
        RawVcard::new(format!(
            "BEGIN:VCARD\r\nVERSION:{version}\r\nUID:c@example.com\r\nFN:{}\r\nEND:VCARD\r\n",
            "x".repeat(padding)
        )),
        book(),
    )
}

#[test]
fn a_card_states_its_version_and_never_prints_its_text() {
    assert_eq!(card("4.0", 1).version(), Some(VcardVersion::V4));
    assert_eq!(card("3.0", 1).version(), Some(VcardVersion::V3));
    assert_eq!(card("9.9", 1).version(), None);
    let bare = ContactImport::new(
        RawVcard::new("BEGIN:VCARD\r\nFN:Private\r\nEND:VCARD"),
        book(),
    );
    assert_eq!(bare.version(), None);
    assert!(!format!("{bare:?}").contains("Private"));
}

#[test]
fn a_receipt_says_whether_the_book_already_held_the_uid() {
    let key = ContactId::try_from("/card/u/friends/c.vcf").unwrap();
    let etag = RevisionTokens::from_etag(ETag::new("\"1\""));
    let created = ImportedContact::created(key.clone(), etag.clone());
    assert_eq!(
        (created.key, created.already_there, created.revisions),
        (Some(key), false, etag)
    );
    let existing = ImportedContact::existing(None);
    assert!(existing.already_there && existing.key.is_none());
}

#[test]
fn a_book_without_limits_takes_any_card() {
    let limits = ContactImportLimits::new();
    assert_eq!(limits.max_card_size(), None);
    assert_eq!(limits.versions(), None);
    assert!(limits.check(&card("2.1", 5_000_000)).is_ok());
}

#[test]
fn a_card_over_the_size_or_in_a_version_the_book_does_not_take_is_never_taken() {
    let limits = ContactImportLimits::new()
        .with_max_card_size(NonZeroU64::new(200).unwrap())
        .with_versions(BTreeSet::from([VcardVersion::V3, VcardVersion::V4]));

    assert!(limits.check(&card("4.0", 1)).is_ok());
    assert!(limits.check(&card("3.0", 1)).is_ok());
    for refused in [card("4.0", 500), card("2.1", 1), card("9.9", 1)] {
        let error = limits.check(&refused).unwrap_err();
        assert!(error.is_item_unavailable());
        assert_eq!(error.class(), FailureClass::Permanent);
    }
    assert_eq!(limits.max_card_size(), NonZeroU64::new(200));
    assert_eq!(limits.versions().map(BTreeSet::len), Some(2));
}

struct Fixed;

#[async_trait]
impl ContactImporter for Fixed {
    async fn contact_import_limits(
        &self,
        _account: &AccountId,
        _book: &AddressBookId,
    ) -> ProviderResult<ContactImportLimits> {
        Ok(ContactImportLimits::new().with_max_card_size(NonZeroU64::new(9).unwrap()))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        _book: &AddressBookId,
    ) -> ProviderResult<SyncState> {
        Ok(SyncState::new("b1"))
    }

    async fn import_contacts(
        &self,
        _account: &AccountId,
        batch: &[ContactImport],
    ) -> Vec<ProviderResult<ImportedContact>> {
        batch
            .iter()
            .map(|_| Ok(ImportedContact::existing(None)))
            .collect()
    }
}

#[async_trait]
impl AddressBookCollections for Fixed {
    async fn create_address_book(
        &self,
        _account: &AccountId,
        new: &NewAddressBook,
    ) -> ProviderResult<AddressBook> {
        let mut made = AddressBook::new(book(), new.name.clone(), ContactSourceClass::Personal);
        made.description.clone_from(&new.description);
        Ok(made)
    }
}

async fn drive(importer: &dyn ContactImporter) -> (Option<NonZeroU64>, SyncState, usize) {
    let limits = importer
        .contact_import_limits(&account(), &book())
        .await
        .unwrap();
    let cursor = importer.import_cursor(&account(), &book()).await.unwrap();
    let answers = importer
        .import_contacts(&account(), &[card("4.0", 1), card("3.0", 1)])
        .await;
    (limits.max_card_size(), cursor, answers.len())
}

#[tokio::test]
async fn a_boxed_or_shared_importer_and_creator_delegate() {
    let expected = (NonZeroU64::new(9), SyncState::new("b1"), 2);
    let boxed: Box<dyn ContactImporter> = Box::new(Fixed);
    let shared: Arc<dyn ContactImporter> = Arc::new(Fixed);
    assert_eq!(drive(&boxed).await, expected);
    assert_eq!(drive(&shared).await, expected);

    let new = NewAddressBook::new("Friends").with_description("Imported");
    let boxed: Box<dyn AddressBookCollections> = Box::new(Fixed);
    let shared: Arc<dyn AddressBookCollections> = Arc::new(Fixed);
    for creator in [&boxed as &dyn AddressBookCollections, &shared] {
        let made = creator.create_address_book(&account(), &new).await.unwrap();
        assert_eq!(made.name, "Friends");
        assert_eq!(made.description.as_deref(), Some("Imported"));
    }
}

/// An adapter that imports cards and creates books, reached only through `Provider`.
struct ContactTarget {
    inner: crate::tests::FakeJmap,
}

#[async_trait]
impl crate::Provider for ContactTarget {
    fn connection_info(&self) -> crate::ConnectionInfo {
        self.inner.connection_info()
    }

    fn contact_importer(&self) -> Option<&dyn ContactImporter> {
        Some(&Fixed)
    }

    fn address_book_collections(&self) -> Option<&dyn AddressBookCollections> {
        Some(&Fixed)
    }
}

impl crate::CalendarSync for ContactTarget {}
impl crate::MailboxWrites for ContactTarget {}
impl crate::CalendarWrites for ContactTarget {}

fn fake() -> crate::tests::FakeJmap {
    crate::tests::FakeJmap {
        info: crate::ConnectionInfo::new(crate::Capabilities::none().with_contacts()),
    }
}

/// Through code generic over `P: Provider`, which is the `Box<dyn Provider>` impl's delegation.
fn reach<P: crate::Provider + ?Sized>(
    provider: &P,
) -> (
    Option<&dyn ContactImporter>,
    Option<&dyn AddressBookCollections>,
) {
    (
        provider.contact_importer(),
        provider.address_book_collections(),
    )
}

#[tokio::test]
async fn a_host_holding_a_boxed_provider_reaches_both_through_the_accessors() {
    let held: Box<dyn crate::Provider> = Box::new(ContactTarget { inner: fake() });
    let (importer, creator) = reach(&held);
    assert_eq!(drive(importer.expect("an importer")).await.2, 2);
    assert!(creator.is_some());

    let plain: Box<dyn crate::Provider> = Box::new(fake());
    let (importer, creator) = reach(&plain);
    assert!(importer.is_none() && creator.is_none());
}
