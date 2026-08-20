//! On-demand contact-photo fetch and its cache keys.
//!
//! Split out of `contacts.rs` by responsibility: the two keys below (which resource,
//! and is it still fresh) are the whole of the caching contract, and they are easy to
//! get subtly wrong — see the doc comments.

use core::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactResource},
    ids::AccountId,
};
use engine_provider::ContactsProvider;
use engine_store::{
    CachedContactPhoto, ContactPhotoCache, ContactPhotoFile, ContactStore, PhotoCacheTtl,
};

use crate::{ApiError, Engine};

/// How long "this card resource has no photo" is trusted before the provider is
/// asked again.
///
/// Long, because the answer almost never changes: a correspondent outside the user's
/// address books has no picture anywhere, and re-probing them costs a request per
/// stranger per pass. Not forever, because a colleague who uploads one should get it
/// without reconnecting the account. A week is well inside the tolerance of both.
const NEGATIVE_TTL: Duration = Duration::from_hours(24 * 7);

/// How long a photo is trusted when **nothing on the card would change if the picture
/// did**.
///
/// A Graph directory user is the case this exists for: `/users` carries no ETag or
/// `changeKey`, the photo resource has no URI of its own, and `/users/delta` neither
/// reports a photo change nor can even name the property (measured — see
/// `docs/agent-guidance/graph.md`). Nothing anywhere says the picture moved, so the only
/// remaining question is how long to keep believing an answer, and three days trades a
/// little bandwidth for a colleague's new photo appearing without a reconnect.
///
/// It applies *only* to that case. A fingerprint that does track the picture invalidates
/// on its own, and re-fetching then would be bytes spent to learn nothing.
const UNREVISIONED_MAX_AGE: Duration = Duration::from_hours(24 * 3);

/// The cache key for one card resource, and whether it can notice the picture changing.
struct PhotoKey {
    fingerprint: String,
    /// `None` when [`photo_fingerprint`] found a real revision.
    max_age: Option<Duration>,
}

impl PhotoKey {
    fn of(card: &ContactCard, media: &ContactResource) -> Self {
        match photo_fingerprint(card, media) {
            Some(fingerprint) => Self {
                fingerprint,
                max_age: None,
            },
            // Still keyed per card and resource, so one person's photo cannot serve
            // another's; it simply cannot be told apart from a *later* photo on the same
            // card, which is exactly what the age bound stands in for.
            None => Self {
                fingerprint: "unrevisioned".to_owned(),
                max_age: Some(UNREVISIONED_MAX_AGE),
            },
        }
    }

    fn ttl(&self) -> PhotoCacheTtl {
        PhotoCacheTtl {
            negative: NEGATIVE_TTL,
            unrevisioned: self.max_age,
        }
    }
}

impl Engine {
    /// Returns the cached file for one card's media resource, fetching it once if
    /// the cache has no answer.
    ///
    /// `None` means the source has no image here — not that the fetch failed. That
    /// answer is remembered for a week, so a mailbox full of strangers costs one
    /// provider request per stranger rather than one per pass.
    ///
    /// The bytes stay on disk and a path comes back: every mail row on screen carries
    /// one of these, and copying each image through this API only for a host to write
    /// it somewhere else is work no one needs. See [`ContactPhotoFile`].
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
    ) -> Result<Option<ContactPhotoFile>, ApiError> {
        let key = PhotoKey::of(card, media);
        let resource = photo_resource_key(media);
        // The file first, because that read is metadata only. Asking the byte-returning
        // cache would load and re-hash the whole image just to discard it and look the
        // path up anyway.
        if let Some(file) = self.photo_file(account, card, &resource, &key).await? {
            return Ok(Some(file));
        }
        // Nothing to serve. Only a *recorded absence* stops us asking; a `Hit` cannot
        // occur here, since the read above would have answered it.
        if matches!(
            self.store
                .contact_photo(account, &card.id, &resource, &key.fingerprint, key.ttl())
                .await?,
            ContactPhotoCache::NoPhoto
        ) {
            return Ok(None);
        }
        let photo = provider
            .fetch_contact_photo(account, card, media)
            .await
            .map_err(|error| ApiError::Sync(engine_sync::SyncError::Provider(error)))?;
        let Some(photo) = photo else {
            self.store
                .put_contact_photo_absent(account, &card.id, &resource, &key.fingerprint)
                .await?;
            return Ok(None);
        };
        self.store
            .put_contact_photo(
                account,
                &card.id,
                &resource,
                &CachedContactPhoto::new(
                    photo.as_bytes().to_vec(),
                    photo.media_type.clone(),
                    key.fingerprint.clone(),
                ),
            )
            .await?;
        self.photo_file(account, card, &resource, &key).await
    }

    /// Returns the cached file for one card's media resource **without** reaching a
    /// provider, or `None` when the cache cannot answer from what it already holds.
    ///
    /// This is the call a host makes while building a screen: it does store work
    /// only, so it belongs on a path that must not grow a network fetch. Whatever it
    /// answers `None` for is what a background pass then resolves through
    /// [`contact_photo`](Self::contact_photo).
    ///
    /// # Errors
    ///
    /// Returns [`ApiError::Store`] when the cache cannot be read.
    pub async fn cached_contact_photo(
        &self,
        account: &AccountId,
        card: &ContactCard,
        media: &ContactResource,
    ) -> Result<Option<ContactPhotoFile>, ApiError> {
        let key = PhotoKey::of(card, media);
        let resource = photo_resource_key(media);
        self.photo_file(account, card, &resource, &key).await
    }

    async fn photo_file(
        &self,
        account: &AccountId,
        card: &ContactCard,
        resource: &str,
        key: &PhotoKey,
    ) -> Result<Option<ContactPhotoFile>, ApiError> {
        Ok(self
            .store
            .contact_photo_file(account, &card.id, resource, &key.fingerprint, key.max_age)
            .await?)
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
    uri_digest(&media.uri)
}

/// Answers "is the cached photo still the one this card points at", or `None` when
/// **nothing on the card could answer that**.
///
/// The last resort is the URI, because a source that versions neither the media nor the
/// card still changes the URI when the photo changes. It is hashed for the same reason
/// [`photo_resource_key`] hashes it: a CardDAV inline `PHOTO;ENCODING=b` *is* a `data:`
/// URI holding the whole image, and storing that as the fingerprint would write a second
/// copy of the image into a column that is string-compared on every read.
///
/// An **empty** URI is where that last resort runs out. It means the card advertises a
/// photo endpoint the adapter derives from the card id rather than a resource with an
/// identity of its own — a Graph directory user — so hashing it yields the same constant
/// for every such card forever. Returning `None` rather than that constant is the whole
/// point: it is the difference between a fingerprint that says "unchanged" and one that
/// has no idea, and only the caller that can tell them apart can decide to bound the
/// entry by age instead.
fn photo_fingerprint(card: &ContactCard, media: &ContactResource) -> Option<String> {
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
        .or_else(|| (!media.uri.is_empty()).then(|| format!("uri:{}", uri_digest(&media.uri))))
}

/// FNV-1a over a resource URI. Both photo cache keys are card-local and compared for
/// equality only, so a 64-bit digest is enough to tell one resource (or one revision
/// of one) from another without carrying the URI itself.
fn uri_digest(uri: &str) -> String {
    let digest = uri.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        hash.wrapping_mul(0x0100_0000_01b3) ^ u64::from(byte)
    });
    format!("{digest:016x}")
}

#[cfg(test)]
mod tests {
    use engine_core::{
        contact::{ContactCard, ContactResource},
        ids::{AddressBookId, ContactId},
        membership::Memberships,
        version::{ETag, RevisionTokens},
    };

    use super::{PhotoKey, UNREVISIONED_MAX_AGE, photo_fingerprint, photo_resource_key};

    fn inline(payload: &str) -> ContactResource {
        ContactResource {
            uri: format!("data:image/jpeg;base64,{payload}"),
            ..ContactResource::default()
        }
    }

    fn card() -> ContactCard {
        ContactCard::new(
            ContactId::try_from("/book/ada.vcf").unwrap(),
            Memberships::of_one(AddressBookId::try_from("/book/").unwrap()),
        )
    }

    /// Both keys are written to a cache row and string-compared on every read, so
    /// neither may carry a `data:` URI's payload — a vCard inline photo is the whole
    /// image, and a megabyte-wide key column is a second copy of it.
    #[test]
    fn both_photo_cache_keys_stay_short_for_an_inline_image() {
        let big = inline(&"A".repeat(64 * 1024));
        let fingerprint = photo_fingerprint(&card(), &big).expect("an inline image is its own key");
        for key in [photo_resource_key(&big), fingerprint] {
            assert!(key.len() <= 32, "{} bytes: {key}", key.len());
        }
        // Still a *fingerprint*: a different inline image must not match.
        assert_ne!(
            photo_fingerprint(&card(), &big),
            photo_fingerprint(&card(), &inline("Qk0=")),
        );
    }

    /// The fingerprint and the age bound are one decision, and splitting them is how the
    /// bound goes missing: a card with nothing to notice a photo change by must come back
    /// with an expiry, and one that has a real revision must not, or every unchanged
    /// picture is re-fetched on a timer for nothing.
    #[test]
    fn only_a_card_that_cannot_notice_a_change_gets_an_age_bound() {
        let endpoint = ContactResource {
            kind: Some("photo".into()),
            ..ContactResource::default()
        };
        let unrevisioned = PhotoKey::of(&card(), &endpoint);
        assert_eq!(unrevisioned.max_age, Some(UNREVISIONED_MAX_AGE));
        assert_eq!(unrevisioned.ttl().unrevisioned, Some(UNREVISIONED_MAX_AGE));

        let mut revised = card();
        revised.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
        let revisioned = PhotoKey::of(&revised, &endpoint);
        assert_eq!(revisioned.max_age, None);
        assert_eq!(
            revisioned.ttl().unrevisioned,
            None,
            "a revision that tracks the picture invalidates on its own"
        );

        // An inline image is its own revision, so it needs no bound either.
        assert_eq!(PhotoKey::of(&card(), &inline("Qk0=")).max_age, None);
    }

    /// A card that advertises a photo endpoint rather than a resource — Graph derives the
    /// URL from the card id, so the media carries no URI — has nothing that could change
    /// when the picture does. Answering with a fingerprint anyway would claim "unchanged"
    /// forever; `None` is what lets the caller bound the entry by age instead.
    #[test]
    fn a_card_with_no_revision_and_no_uri_has_no_fingerprint() {
        let endpoint = ContactResource {
            kind: Some("photo".into()),
            ..ContactResource::default()
        };
        assert_eq!(photo_fingerprint(&card(), &endpoint), None);

        // Any one of the three revisions is enough to make it answerable again.
        let mut revised = card();
        revised.revisions = RevisionTokens::from_etag(ETag::new("\"v1\""));
        assert_eq!(
            photo_fingerprint(&revised, &endpoint).as_deref(),
            Some("etag:\"v1\"")
        );
    }
}
