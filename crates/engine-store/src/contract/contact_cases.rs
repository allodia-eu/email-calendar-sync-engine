//! Contact/people/recipient derived-store cases shared by every backend.

use engine_core::{
    contact::{ContactCard, ContactEmail, ContactProperty, PropertyId},
    ids::{AccountId, AddressBookId, ContactId, MailboxId, MessageId, ProviderKey},
    mail::Message,
    membership::Memberships,
    people::{CanonicalEmail, PeopleSnapshot},
    recipient::{RecipientCoverage, RecipientObservation},
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate, SyncWindow},
};

use super::lease_request;
use crate::{ApplyBatch, CachedContactPhoto, ContactStore, DerivedWrite, Store};

fn account() -> AccountId {
    AccountId::try_from("contact-contract").unwrap()
}

fn card(id: &str, email: &str) -> ContactCard {
    let mut card = ContactCard::new(
        ContactId::try_from(id).unwrap(),
        Memberships::of_one(AddressBookId::try_from("book").unwrap()),
    );
    card.emails.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new(email)),
    );
    card
}

async fn apply_contacts<S>(store: &S, objects: Vec<ContactCard>, cursor: &str)
where
    S: Store,
{
    let scope = SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::ContactCard,
    };
    let claim = store
        .claim_sync_scope(account(), &scope, lease_request("contacts", 60))
        .await
        .unwrap();
    let present = objects.iter().map(|item| item.id.key().clone()).collect();
    let update = SyncUpdate::snapshot(objects, present);
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(
                &update,
                &DerivedWrite::empty(),
                &[],
                &SyncState::new(cursor),
            ),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();
}

pub(super) async fn contact_generation_and_people_cas<S>(store: &S)
where
    S: Store + ContactStore,
{
    assert_eq!(store.contact_sources().await.unwrap().generation, 0);
    apply_contacts(store, vec![card("c1", "one@example.test")], "c1").await;

    let sources = store.contact_sources().await.unwrap();
    assert_eq!(sources.generation, 1);
    assert_eq!(sources.sources.len(), 1);
    assert_eq!(sources.sources[0].id.account, account());

    let first = PeopleSnapshot::empty();
    assert!(
        store
            .replace_people(sources.generation, &first)
            .await
            .unwrap()
    );
    apply_contacts(
        store,
        vec![
            card("c1", "one@example.test"),
            card("c2", "two@example.test"),
        ],
        "c2",
    )
    .await;
    assert!(
        !store
            .replace_people(1, &PeopleSnapshot::empty())
            .await
            .unwrap()
    );
    assert_eq!(store.people_snapshot().await.unwrap(), first);
}

pub(super) async fn recipient_idempotency_and_suppression<S>(store: &S)
where
    S: Store + ContactStore,
{
    let scope = SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    };
    let claim = store
        .claim_sync_scope(account(), &scope, lease_request("recipients", 60))
        .await
        .unwrap();
    let message = Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::of_one(MailboxId::try_from("sent").unwrap()),
    );
    let update = SyncUpdate::delta(vec![message], Vec::<ProviderKey>::new());
    let observation = RecipientObservation {
        account: account(),
        source_message: MessageId::try_from("m1").unwrap(),
        email: CanonicalEmail::parse("friend@example.test").unwrap(),
        name: Some("Friend".into()),
        sent_at: Some("2026-01-01T00:00:00Z".parse().unwrap()),
    };
    let state = SyncState::new("mail-1");
    let derived = DerivedWrite::empty();
    for _ in 0..2 {
        let batch = ApplyBatch::new(&update, &derived, &[], &state)
            .with_recipient_observations(std::slice::from_ref(&observation));
        store.apply_sync_update(&claim.lease, batch).await.unwrap();
    }
    assert_eq!(
        store.recipient_interactions(None).await.unwrap()[0].sent_count,
        1
    );

    store.forget_recipient(&observation.email).await.unwrap();
    let replay = ApplyBatch::new(&update, &derived, &[], &state)
        .with_recipient_observations(std::slice::from_ref(&observation));
    store.apply_sync_update(&claim.lease, replay).await.unwrap();
    assert!(store.recipient_interactions(None).await.unwrap().is_empty());

    // No backfill has run yet, so there is no version to read. Callers rely on this
    // being cheap and truthful: it is what lets them skip the whole-mailbox scan that
    // feeds `apply_recipient_backfill` once the work is already done.
    assert_eq!(
        store.recipient_index_version(&account()).await.unwrap(),
        None
    );

    let mut backfilled = observation.clone();
    backfilled.source_message = MessageId::try_from("m2").unwrap();
    assert!(
        store
            .apply_recipient_backfill(account(), 1, &[backfilled])
            .await
            .unwrap()
    );
    // The applied version is now readable without touching any message row.
    assert_eq!(
        store.recipient_index_version(&account()).await.unwrap(),
        Some(1)
    );
    assert!(
        !store
            .apply_recipient_backfill(account(), 1, &[])
            .await
            .unwrap()
    );
    // A rejected re-apply must not move the version backwards.
    assert_eq!(
        store.recipient_index_version(&account()).await.unwrap(),
        Some(1)
    );
    assert_eq!(
        store.recipient_interactions(None).await.unwrap()[0].sent_count,
        1
    );

    let coverage = RecipientCoverage {
        account: account(),
        window: SyncWindow::full(),
        sent_collection_identified: true,
    };
    store.set_recipient_coverage(&coverage).await.unwrap();
    assert_eq!(
        store.recipient_coverage(Some(account())).await.unwrap(),
        vec![coverage]
    );
}

pub(super) async fn contact_photo_cache_is_fingerprint_bound<S>(store: &S)
where
    S: ContactStore,
{
    let contact = ContactId::try_from("photo-card").unwrap();
    assert!(
        store
            .contact_photo(&account(), &contact, "res-a", "rev-1")
            .await
            .unwrap()
            .is_none()
    );
    let photo = CachedContactPhoto::new(vec![0xff, 0xd8, 0xff], Some("image/jpeg".into()), "rev-1");
    store
        .put_contact_photo(&account(), &contact, "res-a", &photo)
        .await
        .unwrap();
    assert_eq!(
        store
            .contact_photo(&account(), &contact, "res-a", "rev-1")
            .await
            .unwrap(),
        Some(photo)
    );
    assert!(
        store
            .contact_photo(&account(), &contact, "res-a", "rev-2")
            .await
            .unwrap()
            .is_none(),
        "a changed revision must invalidate cached bytes"
    );
}

/// A card may carry several media resources (a `PHOTO` and a `LOGO`), and the
/// fingerprint cannot tell them apart — its fallback is the *card's* ETag, identical
/// for every resource on that card. Each resource therefore needs its own cache entry:
/// keyed by card alone, a `LOGO` fetch would satisfy a later `PHOTO` read and hand back
/// the wrong bytes.
pub(super) async fn contact_photo_cache_separates_resources_on_one_card<S>(store: &S)
where
    S: ContactStore,
{
    let contact = ContactId::try_from("multi-media-card").unwrap();
    // Same card, same fingerprint (the shared card ETag), two different resources.
    let logo = CachedContactPhoto::new(b"logo-bytes".to_vec(), Some("image/png".into()), "etag:v1");
    let photo = CachedContactPhoto::new(
        b"photo-bytes".to_vec(),
        Some("image/jpeg".into()),
        "etag:v1",
    );
    store
        .put_contact_photo(&account(), &contact, "logo", &logo)
        .await
        .unwrap();
    store
        .put_contact_photo(&account(), &contact, "photo", &photo)
        .await
        .unwrap();

    // Neither read may return the other's bytes, and storing the second must not have
    // evicted the first.
    assert_eq!(
        store
            .contact_photo(&account(), &contact, "logo", "etag:v1")
            .await
            .unwrap(),
        Some(logo)
    );
    assert_eq!(
        store
            .contact_photo(&account(), &contact, "photo", "etag:v1")
            .await
            .unwrap(),
        Some(photo)
    );
    // An unknown resource on a cached card is a miss, not a wrong-bytes hit.
    assert!(
        store
            .contact_photo(&account(), &contact, "sound", "etag:v1")
            .await
            .unwrap()
            .is_none()
    );
}
