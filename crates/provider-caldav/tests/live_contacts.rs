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
