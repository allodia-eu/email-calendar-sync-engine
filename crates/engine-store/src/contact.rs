//! Contact-source generations, people-index CAS, and recipient history.

use core::time::Duration;
use std::{collections::BTreeMap, path::PathBuf};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ContactId},
    people::{CanonicalEmail, PeopleSnapshot, Person, PersonSource},
    recipient::{RecipientCoverage, RecipientInteraction, RecipientObservation},
    sync::SyncScope,
};

use crate::Result;

/// Cached contact-photo bytes bound to a provider revision/media fingerprint.
#[derive(Clone, PartialEq, Eq)]
pub struct CachedContactPhoto {
    bytes: Box<[u8]>,
    /// Media type, when known.
    pub media_type: Option<String>,
    /// Provider revision/media fingerprint that validates these bytes.
    pub fingerprint: String,
}

impl CachedContactPhoto {
    /// Creates a cache entry.
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

    /// Returns the cached bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the entry into its bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes.into_vec()
    }
}

impl core::fmt::Debug for CachedContactPhoto {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CachedContactPhoto")
            .field("len", &self.bytes.len())
            .field("media_type", &self.media_type)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// What the photo cache holds for one card resource.
///
/// [`NoPhoto`](Self::NoPhoto) is the reason this is not an `Option`. "The provider
/// has no photo for this person" is the common answer for a correspondent outside
/// the user's address books, and it is worth remembering: without a stored negative
/// every pass over a mailing list re-asks the provider about the same strangers.
/// It is remembered with an expiry rather than forever, because a person who adds a
/// profile picture must eventually get one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactPhotoCache {
    /// Cached bytes still bound to the card's current fingerprint.
    Hit(CachedContactPhoto),
    /// The provider answered "there is no photo here", recently enough to trust.
    NoPhoto,
    /// Nothing usable is cached: never asked, superseded, or the negative expired.
    Miss,
}

/// A cached contact photo as a file the host can hand straight to an image decoder.
///
/// The bytes are already on disk in the content-addressed blob area, so a host that
/// draws them — every mail row on screen carries one — passes this path to the
/// platform decoder instead of copying the image through the API and across its own
/// FFI boundary. The file is named by the SHA-256 of its contents, so the name
/// changes when the photo does and a host may cache against it indefinitely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactPhotoFile {
    /// Absolute path to the image bytes.
    pub path: PathBuf,
    /// Media type, when the provider stated one. Untrusted: it is remote content
    /// describing itself, so a host that cares what the bytes are must sniff them.
    pub media_type: Option<String>,
}

/// A consistent read of all live source cards and their generation.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactSourceSnapshot {
    /// Generation observed while reading the source rows.
    pub generation: u64,
    /// Live provider source records.
    pub sources: Vec<PersonSource>,
}

/// Persisted availability of one independently permissioned contact source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactSourceAvailability {
    /// The source was read successfully.
    Available,
    /// The source could not be read independently from sibling sources.
    Unavailable {
        /// Stable explanation, normally a missing permission.
        reason: String,
    },
}

/// Durable derived-contact and interaction operations.
///
/// Source objects themselves continue to use [`Store`](crate::Store). This
/// companion trait owns the generation-CAS people replacement and history
/// suppression semantics that are not generic sync operations.
#[async_trait]
pub trait ContactStore: Send + Sync {
    /// Reads what the cache holds for one card resource.
    ///
    /// `resource` identifies *which* media resource on the card is being read — a
    /// card may carry several (a `PHOTO` and a `LOGO`), and they must not share a
    /// cache entry. A cached photo counts as a hit only while its provider
    /// fingerprint still matches; a recorded absence counts as
    /// [`NoPhoto`](ContactPhotoCache::NoPhoto) only while it is younger than
    /// `negative_ttl`, and reads as [`Miss`](ContactPhotoCache::Miss) after that so
    /// the caller re-asks.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
        negative_ttl: Duration,
    ) -> Result<ContactPhotoCache>;

    /// Stores photo bytes in the content-addressed cache and replaces stale
    /// metadata for this card's `resource` (see [`contact_photo`](Self::contact_photo)).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn put_contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        photo: &CachedContactPhoto,
    ) -> Result<()>;

    /// Records that the provider has no photo for this card resource, replacing any
    /// entry already there.
    ///
    /// Stamped with the store clock so [`contact_photo`](Self::contact_photo) can
    /// expire it, and bound to `fingerprint` so a changed card re-asks immediately
    /// rather than waiting the negative out.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn put_contact_photo_absent(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
    ) -> Result<()>;

    /// Resolves canonical emails to the people carrying them, in one round-trip.
    ///
    /// Addresses with no match are absent from the map rather than present-and-empty.
    /// This is a batch because the caller is a screenful of mail rows: resolving them
    /// one at a time is one query per row, per rebuild.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn people_by_email(
        &self,
        emails: &[CanonicalEmail],
    ) -> Result<BTreeMap<CanonicalEmail, Person>>;

    /// Reads all live contact source records at one generation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn contact_sources(&self) -> Result<ContactSourceSnapshot>;

    /// Reads the current materialized people snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn people_snapshot(&self) -> Result<PeopleSnapshot>;

    /// Atomically replaces people only if the contact-source generation still
    /// equals `expected_generation`.
    ///
    /// Returns `false` on a generation race; callers rebuild from a fresh
    /// [`contact_sources`](Self::contact_sources) read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn replace_people(
        &self,
        expected_generation: u64,
        people: &PeopleSnapshot,
    ) -> Result<bool>;

    /// Aggregates eligible recipient observations, optionally for one account.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn recipient_interactions(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientInteraction>>;

    /// Suppresses existing observations for `email` across accounts.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn forget_recipient(&self, email: &CanonicalEmail) -> Result<usize>;

    /// Suppresses all existing observations for one account.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn clear_recipient_history(&self, account: AccountId) -> Result<usize>;

    /// Suppresses all existing observations.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn clear_all_recipient_history(&self) -> Result<usize>;

    /// Reads the per-account interaction-index version a backfill last advanced to,
    /// or `None` when none has run.
    ///
    /// Callers check this *before* scanning the mailbox: the scan that feeds
    /// [`apply_recipient_backfill`](Self::apply_recipient_backfill) deserializes every
    /// stored message, and without this read that cost is paid on every sync only for
    /// the write to be rejected as already-applied.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn recipient_index_version(&self, account: &AccountId) -> Result<Option<u32>>;

    /// Atomically inserts a one-time backfill and advances its per-account
    /// interaction-index version. Returns `false` when that version already ran.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn apply_recipient_backfill(
        &self,
        account: AccountId,
        version: u32,
        observations: &[RecipientObservation],
    ) -> Result<bool>;

    /// Records the normal mail window and Sent-role detectability for an account.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn set_recipient_coverage(&self, coverage: &RecipientCoverage) -> Result<()>;

    /// Reads recipient coverage, optionally restricted to one account.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn recipient_coverage(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientCoverage>>;

    /// Records one contact source's availability.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn set_contact_source_availability(
        &self,
        scope: &SyncScope,
        availability: &ContactSourceAvailability,
    ) -> Result<()>;

    /// Reads persisted availability for an account's contact sources.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`](crate::StoreError) on a backend failure.
    async fn contact_source_availability(
        &self,
        account: AccountId,
    ) -> Result<Vec<(SyncScope, ContactSourceAvailability)>>;
}
