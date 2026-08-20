//! Async [`ContactStore`] implementation over the SQLite operations.

use core::time::Duration;
use std::collections::BTreeMap;

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ContactId},
    people::{CanonicalEmail, PeopleSnapshot, Person},
    recipient::{RecipientCoverage, RecipientInteraction, RecipientObservation},
    sync::SyncScope,
};
use engine_store::{
    CachedContactPhoto, Clock, ContactPhotoCache, ContactPhotoFile, ContactSourceAvailability,
    ContactSourceSnapshot, ContactStore, PhotoCacheTtl, Result,
};

use crate::{
    SqliteStore, blob, contact_ops,
    convert::{instant_to_text, parse_instant},
    photo_ops,
};

#[async_trait]
impl<C: Clock> ContactStore for SqliteStore<C> {
    async fn contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
        ttl: PhotoCacheTtl,
    ) -> Result<ContactPhotoCache> {
        let now = self.clock.now();
        let fingerprint = fingerprint.to_owned();
        let Some(row) = self
            .select_photo(account, contact, resource, &fingerprint)
            .await?
        else {
            return Ok(ContactPhotoCache::Miss);
        };
        let fetched_at = parse_instant(&row.fetched_at)?;
        let fresh = |window: Duration| {
            fetched_at
                .checked_add(window)
                .is_some_and(|expiry| expiry > now)
        };
        if row.missing {
            return Ok(if fresh(ttl.negative) {
                ContactPhotoCache::NoPhoto
            } else {
                ContactPhotoCache::Miss
            });
        }
        // A stored photo expires only when the caller says its fingerprint cannot notice
        // the picture changing; otherwise a changed fingerprint is what invalidates it.
        if ttl.unrevisioned.is_some_and(|window| !fresh(window)) {
            return Ok(ContactPhotoCache::Miss);
        }
        let root = self.blobs.root().to_path_buf();
        let hash = row.content_hash;
        // An evicted or corrupted blob reads as a miss, not as a photo of nothing.
        Ok(Self::block(move || blob::read_contact_photo(&root, &hash))
            .await?
            .map_or(ContactPhotoCache::Miss, |bytes| {
                ContactPhotoCache::Hit(CachedContactPhoto::new(bytes, row.media_type, fingerprint))
            }))
    }

    async fn put_contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        photo: &CachedContactPhoto,
    ) -> Result<()> {
        let root = self.blobs.root().to_path_buf();
        let bytes = photo.as_bytes().to_vec();
        let hash = Self::block(move || blob::write_contact_photo(&root, &bytes)).await?;
        self.upsert_photo(
            account,
            contact,
            resource,
            PhotoValue {
                fingerprint: &photo.fingerprint,
                content_hash: &hash,
                media_type: photo.media_type.as_deref(),
                missing: false,
            },
        )
        .await
    }

    async fn put_contact_photo_absent(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
    ) -> Result<()> {
        // No bytes, so no blob and no hash naming one.
        self.upsert_photo(
            account,
            contact,
            resource,
            PhotoValue {
                fingerprint,
                content_hash: "",
                media_type: None,
                missing: true,
            },
        )
        .await
    }

    async fn people_by_email(
        &self,
        emails: &[CanonicalEmail],
    ) -> Result<BTreeMap<CanonicalEmail, Person>> {
        let emails: Vec<String> = emails
            .iter()
            .map(|email| email.as_str().to_owned())
            .collect();
        self.read(move |conn| contact_ops::people_by_email(conn, &emails))
            .await
    }

    async fn contact_sources(&self) -> Result<ContactSourceSnapshot> {
        self.call(contact_ops::contact_sources).await
    }

    async fn people_snapshot(&self) -> Result<PeopleSnapshot> {
        self.call(|conn| contact_ops::people_snapshot(conn)).await
    }

    async fn replace_people(
        &self,
        expected_generation: u64,
        people: &PeopleSnapshot,
    ) -> Result<bool> {
        let people = people.clone();
        self.call(move |conn| contact_ops::replace_people(conn, expected_generation, &people))
            .await
    }

    async fn recipient_interactions(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientInteraction>> {
        self.call(move |conn| contact_ops::recipient_interactions(conn, account.as_ref()))
            .await
    }

    async fn forget_recipient(&self, email: &CanonicalEmail) -> Result<usize> {
        let email = email.clone();
        self.call(move |conn| contact_ops::suppress_email(conn, &email))
            .await
    }

    async fn clear_recipient_history(&self, account: AccountId) -> Result<usize> {
        self.call(move |conn| contact_ops::suppress_account(conn, &account))
            .await
    }

    async fn clear_all_recipient_history(&self) -> Result<usize> {
        self.call(|conn| contact_ops::suppress_all(conn)).await
    }

    async fn recipient_index_version(&self, account: &AccountId) -> Result<Option<u32>> {
        let account = account.clone();
        self.call(move |conn| contact_ops::recipient_index_version(conn, &account))
            .await
    }

    async fn apply_recipient_backfill(
        &self,
        account: AccountId,
        version: u32,
        observations: &[RecipientObservation],
    ) -> Result<bool> {
        let observations = observations.to_vec();
        self.call(move |conn| {
            contact_ops::apply_recipient_backfill(conn, &account, version, &observations)
        })
        .await
    }

    async fn set_recipient_coverage(&self, coverage: &RecipientCoverage) -> Result<()> {
        let coverage = coverage.clone();
        self.call(move |conn| contact_ops::set_recipient_coverage(conn, &coverage))
            .await
    }

    async fn recipient_coverage(
        &self,
        account: Option<AccountId>,
    ) -> Result<Vec<RecipientCoverage>> {
        self.call(move |conn| contact_ops::recipient_coverage(conn, account.as_ref()))
            .await
    }

    async fn set_contact_source_availability(
        &self,
        scope: &SyncScope,
        availability: &ContactSourceAvailability,
    ) -> Result<()> {
        let scope = scope.clone();
        let availability = availability.clone();
        self.call(move |conn| contact_ops::set_source_availability(conn, &scope, &availability))
            .await
    }

    async fn contact_source_availability(
        &self,
        account: AccountId,
    ) -> Result<Vec<(SyncScope, ContactSourceAvailability)>> {
        self.call(move |conn| contact_ops::source_availability(conn, &account))
            .await
    }
}

/// What one cache row records, apart from which resource it belongs to.
struct PhotoValue<'a> {
    fingerprint: &'a str,
    /// Names the blob holding the bytes; empty when `missing`.
    content_hash: &'a str,
    media_type: Option<&'a str>,
    /// The provider has no photo for this resource.
    missing: bool,
}

impl<C: Clock> SqliteStore<C> {
    /// The file holding this card resource's cached photo, when the cache holds one
    /// still bound to `fingerprint`.
    ///
    /// Cache-only and metadata-only: it never reaches a provider and never reads the
    /// image, so a host may call it for every row it is about to draw. A recorded
    /// absence answers `None` here whatever its age — expiring the negative is what
    /// decides whether to *re-ask a provider*, and this call asks no one.
    ///
    /// `unrevisioned_max_age` mirrors [`PhotoCacheTtl::unrevisioned`]: `Some` only for a
    /// card carrying no revision that tracks the picture, in which case an entry past
    /// that age answers `None` so the host treats the row as unresolved and its
    /// background pass refreshes it. Passing `None` here for such a card would keep the
    /// first picture ever cached on screen for good, because a host that already has a
    /// path never asks again.
    ///
    /// Existence is the whole check. The blob is named by the SHA-256 of its bytes
    /// and staged through an atomic rename, so a file under that name either holds
    /// those bytes or does not exist; re-hashing it here would mean reading every
    /// image on a path whose reason for returning a path is not to.
    ///
    /// # Errors
    ///
    /// Returns [`engine_store::StoreError::Backend`] on a backend failure.
    pub async fn contact_photo_file(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
        unrevisioned_max_age: Option<Duration>,
    ) -> Result<Option<ContactPhotoFile>> {
        let now = self.clock.now();
        let Some(row) = self
            .select_photo(account, contact, resource, fingerprint)
            .await?
        else {
            return Ok(None);
        };
        if row.missing {
            return Ok(None);
        }
        if let Some(window) = unrevisioned_max_age {
            let expiry = parse_instant(&row.fetched_at)?.checked_add(window);
            if expiry.is_none_or(|expiry| expiry <= now) {
                return Ok(None);
            }
        }
        let path = blob::contact_photo_path(self.blobs.root(), &row.content_hash);
        let media_type = row.media_type;
        Ok(Self::block(move || path.exists().then_some(path))
            .await
            .map(|path| ContactPhotoFile { path, media_type }))
    }

    async fn select_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
    ) -> Result<Option<photo_ops::CachedRow>> {
        let account = account.as_str().to_owned();
        let contact = contact.as_str().to_owned();
        let resource = resource.to_owned();
        let fingerprint = fingerprint.to_owned();
        self.read(move |conn| photo_ops::select(conn, &account, &contact, &resource, &fingerprint))
            .await
    }

    async fn upsert_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        value: PhotoValue<'_>,
    ) -> Result<()> {
        let account = account.as_str().to_owned();
        let contact = contact.as_str().to_owned();
        let resource = resource.to_owned();
        let PhotoValue {
            fingerprint,
            content_hash,
            media_type,
            missing,
        } = value;
        let fingerprint = fingerprint.to_owned();
        let content_hash = content_hash.to_owned();
        let media_type = media_type.map(str::to_owned);
        let fetched_at = instant_to_text(self.clock.now());
        self.call(move |conn| {
            photo_ops::upsert(
                conn,
                &photo_ops::PhotoRow {
                    account: &account,
                    contact: &contact,
                    resource: &resource,
                    fingerprint: &fingerprint,
                    content_hash: &content_hash,
                    media_type: media_type.as_deref(),
                    fetched_at: &fetched_at,
                    missing,
                },
            )
        })
        .await
    }
}
