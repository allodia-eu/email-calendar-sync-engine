//! Contact-photo caching, and the address -> person -> card walk a host makes to
//! reach the card a photo hangs off.

use super::*;

#[tokio::test]
async fn contact_photos_are_cached_until_the_media_fingerprint_changes() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    let card = FakeContacts::card("c1", "Ada", "ada@example.test");
    let mut media = ContactResource {
        uri: "https://photos.test/ada".into(),
        media_type: Some("image/jpeg".into()),
        fingerprint: Some("photo-1".into()),
        ..ContactResource::default()
    };

    let first = engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap()
        .expect("the fake answers with a photo");
    let cached = engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap()
        .expect("still cached");
    assert_eq!(first.path, cached.path);
    assert_eq!(first.media_type.as_deref(), Some("image/jpeg"));
    assert_eq!(provider.photos.load(Ordering::SeqCst), 1);
    // The path is what a host draws from, so it has to name real bytes.
    assert_eq!(
        std::fs::read(&first.path).unwrap(),
        vec![0xff, 0xd8, 0xff],
        "the returned path must hold the fetched image"
    );

    media.fingerprint = Some("photo-2".into());
    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    assert_eq!(provider.photos.load(Ordering::SeqCst), 2);
}

/// The common case: nobody has a photo for this person. It must be *asked once*, and
/// the answer must survive so a mailbox of strangers does not re-probe them on every
/// pass. That is the whole reason absence stopped being an error.
#[tokio::test]
async fn a_card_with_no_photo_is_asked_once_and_then_remembered() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts {
        no_photo: true,
        ..FakeContacts::default()
    };
    let account = AccountId::try_from("account-1").unwrap();
    let card = FakeContacts::card("c1", "Ada", "ada@example.test");
    let mut media = ContactResource {
        uri: "https://photos.test/ada".into(),
        fingerprint: Some("photo-1".into()),
        ..ContactResource::default()
    };

    for _ in 0..3 {
        assert!(
            engine
                .contact_photo(&provider, &account, &card, &media)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        provider.photos.load(Ordering::SeqCst),
        1,
        "a remembered absence must not re-probe the provider"
    );

    // A changed media revision is a different question, and is asked again.
    media.fingerprint = Some("photo-2".into());
    assert!(
        engine
            .contact_photo(&provider, &account, &card, &media)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(provider.photos.load(Ordering::SeqCst), 2);
}

/// The cache-only read is what a host calls while building a screen, so it must never
/// reach a provider — including for a card whose photo has simply not been fetched yet.
#[tokio::test]
async fn the_cache_only_read_never_reaches_a_provider() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    let card = FakeContacts::card("c1", "Ada", "ada@example.test");
    let media = ContactResource {
        uri: "https://photos.test/ada".into(),
        fingerprint: Some("photo-1".into()),
        ..ContactResource::default()
    };

    assert!(
        engine
            .cached_contact_photo(&account, &card, &media)
            .await
            .unwrap()
            .is_none(),
        "nothing is cached yet"
    );
    assert_eq!(
        provider.photos.load(Ordering::SeqCst),
        0,
        "the hot path must do store work only"
    );

    engine
        .contact_photo(&provider, &account, &card, &media)
        .await
        .unwrap();
    let cached = engine
        .cached_contact_photo(&account, &card, &media)
        .await
        .unwrap()
        .expect("the background fetch filled the cache");
    assert!(cached.path.exists());
    assert_eq!(provider.photos.load(Ordering::SeqCst), 1);
}

/// A host resolving a mail row starts at an address and needs the card the photo API
/// takes. Both hops — address to person, person's source to card — are exposed here,
/// and a stranger has to fall out of the first one rather than error.
#[tokio::test]
async fn addresses_resolve_to_people_and_people_to_their_source_cards() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();

    // `CanonicalEmail` case-folds the domain and leaves the local part exact, so the
    // seeded `Ada@Example.COM` keys as `Ada@example.com` — deliberately, since two
    // mailboxes differing only in local-part case may be two people.
    let ada = engine_api::CanonicalEmail::parse("Ada@Example.COM").unwrap();
    let stranger = engine_api::CanonicalEmail::parse("nobody@example.test").unwrap();
    let found = engine
        .people_by_email(&[ada.clone(), stranger])
        .await
        .unwrap();
    assert_eq!(found.len(), 1, "an unknown sender resolves to nobody");

    let person = &found[&ada];
    let source = person.sources.iter().next().expect("a source card");
    let card = engine
        .contact_card(&source.account, &source.contact)
        .await
        .unwrap()
        .expect("the synced card");
    assert_eq!(card.id, source.contact);

    assert!(
        engine
            .contact_card(&account, &ContactId::try_from("never-synced").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}
