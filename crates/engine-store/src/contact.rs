//! Contact-source generations, people-index CAS, and recipient history.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ContactId},
    people::{CanonicalEmail, PeopleSnapshot, PersonSource},
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
    /// Reads a cached photo only when its provider fingerprint still matches.
    ///
    /// `resource` identifies *which* media resource on the card is being read — a
    /// card may carry several (a `PHOTO` and a `LOGO`), and they must not share a
    /// cache entry.
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
    ) -> Result<Option<CachedContactPhoto>>;

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
