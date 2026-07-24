//! On-demand contact-photo fetch and its cache keys.
//!
//! Split out of `contacts.rs` by responsibility: the two keys below (which resource,
//! and is it still fresh) are the whole of the caching contract, and they are easy to
//! get subtly wrong — see the doc comments.

use engine_core::{
    contact::{ContactCard, ContactResource},
    ids::AccountId,
};
use engine_provider::{ContactPhoto, ContactsProvider};
use engine_store::{CachedContactPhoto, ContactStore};

use crate::{ApiError, Engine};

impl Engine {
    /// Authenticated on-demand contact-photo fetch.
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] for cache failures or [`ApiError::Sync`] when
    /// the provider fetch fails.
    pub async fn contact_photo<P: ContactsProvider>(
        &self,
        provider: &P,
        account: &AccountId,
        card: &ContactCard,
        media: &ContactResource,
    ) -> Result<ContactPhoto, ApiError> {
        let fingerprint = photo_fingerprint(card, media);
        let resource = photo_resource_key(media);
        if let Some(cached) = self
            .store
            .contact_photo(account, &card.id, &resource, &fingerprint)
            .await?
        {
            let media_type = cached.media_type.clone();
            let fingerprint = cached.fingerprint.clone();
            return Ok(ContactPhoto::new(
                cached.into_bytes(),
                media_type,
                fingerprint,
            ));
        }
        let photo = provider
            .fetch_contact_photo(account, card, media)
            .await
            .map_err(|error| ApiError::Sync(engine_sync::SyncError::Provider(error)))?;
        self.store
            .put_contact_photo(
                account,
                &card.id,
                &resource,
                &CachedContactPhoto::new(
                    photo.as_bytes().to_vec(),
                    photo.media_type.clone(),
                    fingerprint,
                ),
            )
            .await?;
        Ok(photo)
    }
}

/// Identifies *which* media resource on a card a cache entry belongs to.
///
/// A card can carry several (`PHOTO`, `LOGO`, `SOUND` all land in
/// `ContactCard::media`), and the fingerprint alone cannot separate them: its ETag
/// fallback is the card's, identical for every resource on that card. Without this
/// discriminator a `LOGO` fetch would satisfy a later `PHOTO` read.
///
/// The URI is hashed rather than stored verbatim so a `data:` URI — which can be
/// megabytes — does not become a primary-key column. Only resources on the *same*
/// card share a key space, so a 64-bit digest is ample.
fn photo_resource_key(media: &ContactResource) -> String {
    let digest = media
        .uri
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            hash.wrapping_mul(0x0100_0000_01b3) ^ u64::from(byte)
        });
    format!("{digest:016x}")
}

fn photo_fingerprint(card: &ContactCard, media: &ContactResource) -> String {
    media
        .fingerprint
        .as_ref()
        .map(|value| format!("media:{value}"))
        .or_else(|| {
            card.revisions
                .etag
                .as_ref()
                .map(|value| format!("etag:{}", value.as_str()))
        })
        .or_else(|| {
            card.revisions
                .change_key
                .as_ref()
                .map(|value| format!("change-key:{}", value.as_str()))
        })
        .unwrap_or_else(|| format!("uri:{}", media.uri))
}
