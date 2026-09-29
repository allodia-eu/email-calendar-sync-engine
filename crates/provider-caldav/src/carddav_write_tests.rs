//! CardDAV reads and writes of one card by href, and the ways a write fails closed.
//!
//! Split from `carddav_tests.rs`, which had reached the size limit; the fixtures are shared.

use std::sync::Arc;

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactField, ContactName, ContactPatch, ContactResource,
        FieldPatch,
    },
    error::FailureClass,
    ids::{AddressBookId, ContactId},
    membership::Memberships,
    version::{ETag, RevisionTokens},
};
use engine_provider::{ContactsProvider, IgnoreConnectSteps};

use crate::{
    CardDavProvider,
    carddav_tests::{BOOKS, CONTACTS, PRINCIPAL, READ_ONLY_BOOKS, account},
    test_support::{Replay, ok, options, status, wrote},
    transport::Precondition,
};

#[tokio::test]
async fn direct_fetch_patch_delete_and_photos_use_exact_guards() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(CONTACTS),
        wrote(204, Some("\"v2\"")),
        wrote(404, None),
        status(200, "uri-photo"),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    let mut card = provider
        .fetch_contact(
            &account(),
            &ContactId::try_from("/dav/addressbooks/alice/default/ada.vcf").unwrap(),
        )
        .await
        .unwrap();
    let multiget = replay.reads().last().unwrap().3.clone();
    assert!(multiget.contains("<d:href>/dav/addressbooks/alice/default/ada.vcf</d:href>"));

    let mut patch = ContactPatch::default();
    patch.fields.insert(
        ContactField::Name,
        FieldPatch::Set(
            serde_json::to_value(ContactName {
                full: Some("Ada Updated".into()),
                ..ContactName::default()
            })
            .unwrap(),
        ),
    );
    provider
        .patch_contact(&account(), &card, &patch)
        .await
        .unwrap();
    provider.delete_contact(&account(), &card).await.unwrap();
    {
        let writes = replay.writes();
        assert_eq!(
            writes[0].precondition,
            Precondition::IfMatch("\"v1\"".into())
        );
        assert_eq!(
            writes[1].precondition,
            Precondition::IfMatch("\"v1\"".into())
        );
    }

    let embedded = provider
        .fetch_contact_photo(
            &account(),
            &card,
            &ContactResource {
                uri: "data:image/jpeg;base64,AQID".into(),
                media_type: Some("image/jpeg".into()),
                ..ContactResource::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(embedded.expect("inline photo").as_bytes(), &[1, 2, 3]);
    let remote = provider
        .fetch_contact_photo(
            &account(),
            &card,
            &ContactResource {
                uri: "https://contacts.example/photo".into(),
                ..ContactResource::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(remote.expect("remote photo").as_bytes(), b"uri-photo");

    card.revisions = RevisionTokens::none();
    assert!(
        provider
            .patch_contact(&account(), &card, &ContactPatch::default())
            .await
            .is_err()
    );
    assert!(provider.delete_contact(&account(), &card).await.is_err());
}

#[tokio::test]
async fn read_only_wrong_destination_conflicts_and_malformed_results_fail_closed() {
    let read_only = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(READ_ONLY_BOOKS),
        ])),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    assert!(read_only.contact_destination().is_none());
    let book = AddressBookId::try_from("/dav/addressbooks/alice/default/").unwrap();
    let card = ContactCard::new(
        ContactId::try_from("card").unwrap(),
        Memberships::of_one(book.clone()),
    );
    assert!(
        read_only
            .create_contact(
                &account(),
                &ContactDraft {
                    address_book: book,
                    card: card.clone(),
                }
            )
            .await
            .is_err()
    );

    let conflict = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
            wrote(412, None),
        ])),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    let mut guarded = card.clone();
    guarded.id = ContactId::try_from("/dav/addressbooks/alice/default/card.vcf").unwrap();
    guarded.revisions = RevisionTokens::from_etag(ETag::new("\"old\""));
    guarded.raw_vcard = Some(engine_core::raw::RawVcard::new(
        "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Old\r\nEND:VCARD\r\n",
    ));
    let error = conflict
        .patch_contact(&account(), &guarded, &ContactPatch::default())
        .await
        .unwrap_err();
    assert_eq!(error.class(), FailureClass::Conflict);

    let malformed = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
            ok("<D:multistatus xmlns:D=\"DAV:\"><D:sync-token>x</D:sync-token></D:multistatus>"),
        ])),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .unwrap();
    assert!(
        malformed
            .fetch_contact(&account(), &guarded.id)
            .await
            .is_err()
    );
}
