//! Async [`ContactStore`] implementation over the SQLite operations.

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, ContactId},
    people::{CanonicalEmail, PeopleSnapshot},
    recipient::{RecipientCoverage, RecipientInteraction, RecipientObservation},
    sync::SyncScope,
};
use engine_store::{
    CachedContactPhoto, Clock, ContactSourceAvailability, ContactSourceSnapshot, ContactStore,
    Result,
};

use crate::{SqliteStore, blob, contact_ops, convert::instant_to_text, photo_ops};

#[async_trait]
impl<C: Clock> ContactStore for SqliteStore<C> {
    async fn contact_photo(
        &self,
        account: &AccountId,
        contact: &ContactId,
        resource: &str,
        fingerprint: &str,
    ) -> Result<Option<CachedContactPhoto>> {
        let account = account.as_str().to_owned();
        let contact = contact.as_str().to_owned();
        let resource = resource.to_owned();
        let fingerprint = fingerprint.to_owned();
        let Some((hash, media_type)) = self
            .call({
                let fingerprint = fingerprint.clone();
                move |conn| photo_ops::select(conn, &account, &contact, &resource, &fingerprint)
            })
            .await?
        else {
            return Ok(None);
        };
        let root = self.blobs.root().to_path_buf();
        Ok(Self::block(move || blob::read_contact_photo(&root, &hash))
            .await?
            .map(|bytes| CachedContactPhoto::new(bytes, media_type, fingerprint)))
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
        let account = account.as_str().to_owned();
        let contact = contact.as_str().to_owned();
        let resource = resource.to_owned();
        let fingerprint = photo.fingerprint.clone();
        let media_type = photo.media_type.clone();
        let fetched_at = instant_to_text(self.clock.now());
        self.call(move |conn| {
            photo_ops::upsert(
                conn,
                &photo_ops::PhotoRow {
                    account: &account,
                    contact: &contact,
                    resource: &resource,
                    fingerprint: &fingerprint,
                    content_hash: &hash,
                    media_type: media_type.as_deref(),
                    fetched_at: &fetched_at,
                },
            )
        })
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
