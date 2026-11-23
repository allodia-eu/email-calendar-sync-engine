//! Importing contacts: what a card import sends, what it answers, the limits an address book
//! states, and the [`ContactImporter`] trait.
//!
//! It is the calendar half's shape, for the same reasons (the `calendar` module): a card names
//! itself by its `UID`, so one the book holds is [already there](ImportedContact::already_there);
//! its state is all inside the vCard, so there is no `kept` until a target that keeps less of it
//! (Graph, Google People, or E9's converter) is built. What a card adds is its **version**: an
//! address book names the vCard versions it takes, and a card in another is refused rather than
//! converted, since converting is a writer's job, not an import's.

use std::{collections::BTreeSet, num::NonZeroU64, sync::Arc};

use async_trait::async_trait;
use engine_core::{
    contact::VcardVersion,
    ids::{AccountId, AddressBookId, ContactId},
    raw::RawVcard,
    sync::SyncState,
    version::RevisionTokens,
};

use crate::{ProviderError, ProviderResult};

/// One card to import: its vCard, exactly as the source held it, and the address book it goes
/// in. `Debug` prints the size, never the text ([`RawVcard`]).
///
/// Built with [`new`](Self::new), so a part added later does not break a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ContactImport {
    /// One vCard (RFC 6350, or 3.0, RFC 2426).
    pub vcard: RawVcard,
    /// The address book to store it in.
    pub address_book: AddressBookId,
}

impl ContactImport {
    /// The card `vcard`, to store in `address_book`.
    #[must_use]
    pub fn new(vcard: RawVcard, address_book: AddressBookId) -> Self {
        Self {
            vcard,
            address_book,
        }
    }

    /// The version the card states in its `VERSION`, if it states one this engine knows.
    #[must_use]
    pub fn version(&self) -> Option<VcardVersion> {
        self.vcard
            .as_str()
            .lines()
            .map(|line| line.trim_end_matches('\r'))
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("VERSION")
                    .then(|| VcardVersion::from_token(value))
                    .flatten()
            })
    }
}

/// What importing one card answers.
///
/// Built with [`created`](Self::created) or [`existing`](Self::existing), so a part added later
/// does not break an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportedContact {
    /// The card the target holds, where its answer names it: the stored card's id, or the one
    /// that already held the `UID`.
    pub key: Option<ContactId>,
    /// Whether the book already held a card with this `UID` and stored nothing new.
    pub already_there: bool,
    /// The revision the target reported for what it stored; empty where it reported none.
    pub revisions: RevisionTokens,
}

impl ImportedContact {
    /// A card the import stored, as `key`, at `revisions`.
    #[must_use]
    pub fn created(key: ContactId, revisions: RevisionTokens) -> Self {
        Self {
            key: Some(key),
            already_there: false,
            revisions,
        }
    }

    /// A card the book already held under the same `UID`, as `key` where the target named it.
    #[must_use]
    pub fn existing(key: Option<ContactId>) -> Self {
        Self {
            key,
            already_there: true,
            revisions: RevisionTokens::none(),
        }
    }
}

/// The limits an address book states for the cards it imports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContactImportLimits {
    max_card_size: Option<NonZeroU64>,
    versions: Option<BTreeSet<VcardVersion>>,
}

impl ContactImportLimits {
    /// A book that states no limit.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The book takes no card larger than `bytes` (builder-style): CardDAV `max-resource-size`
    /// (RFC 6352 §6.2.3).
    #[must_use]
    pub fn with_max_card_size(mut self, bytes: NonZeroU64) -> Self {
        self.max_card_size = Some(bytes);
        self
    }

    /// The book takes cards in `versions` alone (builder-style): CardDAV
    /// `supported-address-data` (RFC 6352 §6.2.2).
    #[must_use]
    pub fn with_versions(mut self, versions: BTreeSet<VcardVersion>) -> Self {
        self.versions = Some(versions);
        self
    }

    /// The largest card the book takes, in bytes, or `None` where it states none.
    #[must_use]
    pub fn max_card_size(&self) -> Option<NonZeroU64> {
        self.max_card_size
    }

    /// The vCard versions the book takes, or `None` where it states none.
    #[must_use]
    pub fn versions(&self) -> Option<&BTreeSet<VcardVersion>> {
        self.versions.as_ref()
    }

    /// Checks `card` against these limits.
    ///
    /// # Errors
    ///
    /// A card larger than the book takes, or in a version the book does not name (a card that
    /// states no version it can be held to included), is [`ProviderError::item_unavailable`]:
    /// the book will never take it as it is, and the rest of the batch is unaffected.
    pub fn check(&self, card: &ContactImport) -> ProviderResult<()> {
        let size = u64::try_from(card.vcard.as_str().len()).unwrap_or(u64::MAX);
        if let Some(max) = self.max_card_size
            && size > max.get()
        {
            return Err(ProviderError::item_unavailable(format!(
                "a {size}-byte card exceeds the address book's {max}-byte limit"
            )));
        }
        if let Some(versions) = &self.versions
            && !card
                .version()
                .is_some_and(|version| versions.contains(&version))
        {
            return Err(ProviderError::item_unavailable(
                "the card is in a vCard version the address book does not take",
            ));
        }
        Ok(())
    }
}

/// Imports cards into a target account's address books.
#[async_trait]
pub trait ContactImporter: Send + Sync {
    /// The limits `address_book` states for what it imports ([`ContactImportLimits`]).
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked, or does not know
    /// `address_book`.
    async fn contact_import_limits(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<ContactImportLimits>;

    /// The target's cursor for `address_book`, read now: a delta from it, through the target's
    /// own contact sync bound to that book, lists every card stored in it after this call.
    ///
    /// # Errors
    ///
    /// Returns a classified [`ProviderError`] when the target cannot be asked, or does not know
    /// `address_book`.
    async fn import_cursor(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<SyncState>;

    /// Imports `batch`, answering each card in order, with the answers of
    /// [`CalendarImporter::import_events`](super::CalendarImporter::import_events): `Ok` stored
    /// or [already there](ImportedContact::already_there) by `UID`;
    /// [`requires_confirmation`](ProviderError::requires_confirmation) may be stored; any other
    /// error not stored, [`item_unavailable`](ProviderError::item_unavailable) never.
    async fn import_contacts(
        &self,
        account: &AccountId,
        batch: &[ContactImport],
    ) -> Vec<ProviderResult<ImportedContact>>;
}

/// A boxed importer is an importer, as for [`MailImporter`](super::MailImporter).
#[async_trait]
impl<T: ContactImporter + ?Sized> ContactImporter for Box<T> {
    async fn contact_import_limits(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<ContactImportLimits> {
        (**self).contact_import_limits(account, address_book).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, address_book).await
    }

    async fn import_contacts(
        &self,
        account: &AccountId,
        batch: &[ContactImport],
    ) -> Vec<ProviderResult<ImportedContact>> {
        (**self).import_contacts(account, batch).await
    }
}

/// A shared importer is an importer, as for [`Box`].
#[async_trait]
impl<T: ContactImporter + ?Sized> ContactImporter for Arc<T> {
    async fn contact_import_limits(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<ContactImportLimits> {
        (**self).contact_import_limits(account, address_book).await
    }

    async fn import_cursor(
        &self,
        account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<SyncState> {
        (**self).import_cursor(account, address_book).await
    }

    async fn import_contacts(
        &self,
        account: &AccountId,
        batch: &[ContactImport],
    ) -> Vec<ProviderResult<ImportedContact>> {
        (**self).import_contacts(account, batch).await
    }
}

#[cfg(test)]
#[path = "contact_tests.rs"]
mod tests;
