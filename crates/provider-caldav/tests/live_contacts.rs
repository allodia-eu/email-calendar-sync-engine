//! Gated JMAP Contacts/CardDAV normalization parity against Stalwart.

use std::time::Duration;

use engine_core::{
    contact::{ContactCard, ContactKind},
    ids::AccountId,
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use provider_caldav::{CardDavConfig, CardDavProvider, Credentials};
use provider_jmap::{Credentials as JmapCredentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn cards(sync: ContactSourceSync<ContactCard>) -> Vec<ContactCard> {
    let ContactSourceSync::Available { sync, .. } = sync else {
        panic!("seed source unavailable");
    };
    match sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    }
}

fn seeded<'a>(items: &'a [ContactCard], uid: &str) -> &'a ContactCard {
    items
        .iter()
        .find(|card| card.uid.as_deref() == Some(uid))
        .expect("seeded card")
}

fn emails(card: &ContactCard) -> std::collections::BTreeSet<String> {
    card.emails
        .values()
        .map(|email| email.value.address.clone())
        .collect()
}

#[tokio::test]
async fn jmap_and_carddav_normalize_the_same_seeded_person() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping contact parity: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let origin = format!("http://{}", harness.http_addr);
    let carddav = CardDavProvider::connect(CardDavConfig::new(
        &origin,
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("CardDAV connect");
    let jmap = JmapProvider::connect(JmapConfig::new(
        origin,
        JmapCredentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("JMAP connect");
    let account = AccountId::try_from("contact-parity").unwrap();
    let carddav_cards = cards(carddav.sync_contacts(&account, None).await.unwrap());
    let jmap_cards = cards(jmap.sync_contacts(&account, None).await.unwrap());
    let dav = seeded(&carddav_cards, "contact-3001@test.local");
    let jmap = seeded(&jmap_cards, "contact-3001@test.local");
    assert_eq!(dav.kind, ContactKind::Individual);
    assert_eq!(dav.kind, jmap.kind);
    assert_eq!(dav.display_name(), jmap.display_name());
    assert_eq!(emails(dav), emails(jmap));
    assert!(dav.raw_vcard.is_some());
    assert!(jmap.raw_jscontact.is_some());

    let dav_group = seeded(&carddav_cards, "group-3002@test.local");
    let jmap_group = seeded(&jmap_cards, "group-3002@test.local");
    assert_eq!(dav_group.kind, ContactKind::Group);
    assert_eq!(dav_group.kind, jmap_group.kind);
    assert_eq!(dav_group.display_name(), jmap_group.display_name());
    let members = |card: &ContactCard| {
        card.members
            .values()
            .map(|member| member.value.uid.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(members(dav_group), members(jmap_group));
}

/// "There is no photo" is now an outcome rather than an error, so a live run has to
/// pin **both** directions — a present photo really arrives, and an absent one really
/// answers `None` — against a real server.
///
/// Pinning only the absence would prove nothing: an adapter that never sends a usable
/// request produces exactly the same `None`, and recording that as server behaviour is
/// the trap `AGENTS.md` describes. So the present case runs first, over the same
/// connection, and its bytes are what show the request shape was right.
#[tokio::test]
async fn a_seeded_photo_arrives_and_a_missing_one_is_an_absence_not_a_failure() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping contact photos: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let origin = format!("http://{}", harness.http_addr);
    let carddav = CardDavProvider::connect(CardDavConfig::new(
        &origin,
        Credentials::Basic {
            username: harness.account.clone(),
            password: harness.password.clone(),
        },
    ))
    .await
    .expect("CardDAV connect");
    let account = AccountId::try_from("contact-photos").unwrap();
    let carddav_cards = cards(carddav.sync_contacts(&account, None).await.unwrap());

    // Present: the seeded card carries an inline `PHOTO;ENCODING=b`, so the vCard the
    // server returned is itself the image.
    let card = seeded(&carddav_cards, "contact-3001@test.local");
    let media = card
        .media
        .values()
        .map(|resource| &resource.value)
        .find(|resource| resource.kind.as_deref() == Some("photo"))
        .expect("the seeded card advertises a photo");
    let photo = carddav
        .fetch_contact_photo(&account, card, media)
        .await
        .expect("the fetch succeeds")
        .expect("a card that advertises a photo has one");
    assert_eq!(
        photo.as_bytes(),
        b"\x89PNG\r\n\x1a\n",
        "the inline PHOTO decodes to the seeded bytes"
    );

    // Absent: a `PHOTO` pointing at a resource this server does not hold. The card
    // still names one, so only asking can settle it — and the answer must be an
    // absence a caller can remember, not an error it has to retry.
    let missing = engine_core::contact::ContactResource {
        uri: format!("{origin}/dav/card/{}/no-such-photo", harness.account),
        kind: Some("photo".into()),
        ..engine_core::contact::ContactResource::default()
    };
    assert!(
        carddav
            .fetch_contact_photo(&account, card, &missing)
            .await
            .expect("a missing photo is not a transport failure")
            .is_none()
    );

    // The same seeded person over JMAP: a different request shape entirely — the card
    // names a `blobId` and the bytes come from the session's `downloadUrl` — so the
    // parity that matters is that both adapters reach the same image.
    let jmap = JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        JmapCredentials::basic(&harness.account, &harness.password),
    ))
    .await
    .expect("JMAP connect");
    let jmap_cards = cards(jmap.sync_contacts(&account, None).await.unwrap());
    let jmap_card = seeded(&jmap_cards, "contact-3001@test.local");
    if let Some(jmap_media) = jmap_card
        .media
        .values()
        .map(|resource| &resource.value)
        .find(|resource| resource.kind.as_deref() == Some("photo"))
    {
        let jmap_photo = jmap
            .fetch_contact_photo(&account, jmap_card, jmap_media)
            .await
            .expect("the blob download succeeds")
            .expect("a card naming a blob has one");
        assert_eq!(
            jmap_photo.as_bytes(),
            photo.as_bytes(),
            "both protocols must reach the same seeded image"
        );
    } else {
        eprintln!("note: Stalwart's JMAP ContactCard advertises no photo media for the seed");
    }

    // A group has no photo to advertise at all, which is the *other* shape of absence:
    // answerable from the card, with no request needed.
    let group = seeded(&carddav_cards, "group-3002@test.local");
    assert!(
        group.media.is_empty(),
        "a card with no PHOTO advertises no media"
    );
}
