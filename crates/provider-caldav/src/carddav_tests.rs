use std::sync::Arc;

use engine_core::{
    contact::{
        ContactCard, ContactDraft, ContactField, ContactName, ContactPatch, ContactResource,
        FieldPatch,
    },
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    sync::{SyncScope, SyncState, SyncUpdate},
    version::{ETag, RevisionTokens},
};
use engine_provider::{ContactSourceSync, ContactsProvider, Provider, WriteGuard};

use crate::{
    CardDavProvider,
    test_support::{Replay, ok, status, wrote},
    transport::{DavMethod, Precondition},
};

const PRINCIPAL: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/</D:href><D:propstat><D:prop><C:addressbook-home-set><D:href>/dav/addressbooks/alice/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
const BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Contacts</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege><D:privilege><D:write-content/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
const CONTACTS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v1"</D:getetag><C:address-data><![CDATA[BEGIN:VCARD
VERSION:4.0
UID:ada
FN:Ada Lovelace
EMAIL;TYPE=work:Ada@Example.COM
X-KEEP:untouched
END:VCARD
]]></C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>token-1</D:sync-token></D:multistatus>"#;
const CTAG: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><CS:getctag>ctag-1</CS:getctag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
const READ_ONLY_BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Read only</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
const DELTA: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v2"</D:getetag><C:address-data><![CDATA[BEGIN:VCARD
VERSION:4.0
UID:ada
FN:Ada Updated
END:VCARD
]]></C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/addressbooks/alice/default/gone.vcf</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>token-2</D:sync-token></D:multistatus>"#;

/// A snapshot whose second response is a card the parser cannot read (here: a
/// response carrying `getetag` but no `address-data`, which is what a server returning
/// a partial `propstat` looks like).
const CONTACTS_ONE_UNPARSEABLE: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v1"</D:getetag><C:address-data><![CDATA[BEGIN:VCARD
VERSION:4.0
UID:ada
FN:Ada Lovelace
END:VCARD
]]></C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/addressbooks/alice/default/bad.vcf</D:href><D:propstat><D:prop><D:getetag>"v9"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>token-1</D:sync-token></D:multistatus>"#;

/// A card the parser chokes on must NOT be tombstoned. A snapshot's `present` set is
/// the store's statement of "everything that exists server-side"; anything missing
/// from it is deleted locally. Deriving it from the cards that happened to *parse*
/// turns one unreadable vCard into silent local data loss, so it is derived from the
/// response hrefs the server actually listed.
#[tokio::test]
async fn a_snapshot_keeps_an_unparseable_card_present_instead_of_tombstoning_it() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        ok(BOOKS),
        ok(CONTACTS_ONE_UNPARSEABLE),
    ]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
            .await
            .unwrap();
    let account = AccountId::try_from("account-1").unwrap();
    let result = provider.sync_contacts(&account, None).await.unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, present } = sync.update else {
        panic!("expected snapshot");
    };
    // Only the readable card is projected …
    assert_eq!(objects.len(), 1);
    // … but BOTH hrefs are present, so the unreadable one survives locally.
    assert_eq!(present.len(), 2);
    assert!(
        present.iter().any(|key| key.as_str().ends_with("/bad.vcf")),
        "unparseable card was tombstoned: {present:?}"
    );
}

#[tokio::test]
async fn carddav_snapshot_preserves_raw_and_sends_rfc6578_shape() {
    let replay = Arc::new(Replay::new(vec![ok(PRINCIPAL), ok(BOOKS), ok(CONTACTS)]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
            .await
            .unwrap();
    let account = AccountId::try_from("account-1").unwrap();
    let result = provider.sync_contacts(&account, None).await.unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 1);
    assert!(objects[0].raw_vcard.is_some());
    assert_eq!(
        objects[0]
            .revisions
            .etag
            .as_ref()
            .map(engine_core::version::ETag::as_str),
        Some("\"v1\"")
    );
    let reads = replay.reads();
    let (method, href, depth, body) = reads.last().unwrap();
    assert_eq!(*method, DavMethod::Report);
    assert_eq!(href, "/dav/addressbooks/alice/default/");
    assert_eq!(depth, "1");
    assert!(body.contains("<d:sync-collection"));
    assert!(body.contains("<a:address-data"));
}

#[tokio::test]
async fn carddav_create_uses_vcard_put_and_if_none_match() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        ok(BOOKS),
        wrote(201, Some("\"v1\"")),
    ]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
            .await
            .unwrap();
    let book = AddressBookId::try_from("/dav/addressbooks/alice/default/").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("ignored").unwrap(),
        Memberships::of_one(book.clone()),
    );
    card.uid = Some("ada@example.test".into());
    provider
        .create_contact(
            &AccountId::try_from("account-1").unwrap(),
            &ContactDraft {
                address_book: book,
                card,
            },
        )
        .await
        .unwrap();
    let writes = replay.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, DavMethod::Put);
    assert_eq!(writes[0].precondition, Precondition::IfNoneMatch);
    assert_eq!(writes[0].content_type, Some("text/vcard; charset=utf-8"));
    assert!(writes[0].href.ends_with("ada%40example.test.vcf"));
    assert!(writes[0].body.contains("UID:ada@example.test"));
}

#[tokio::test]
async fn unsupported_collection_sync_falls_back_to_an_unfiltered_snapshot() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        ok(BOOKS),
        status(405, "sync-collection unsupported"),
        ok(CTAG),
        ok(CONTACTS),
    ]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
            .await
            .unwrap();
    let result = provider
        .sync_contacts(
            &AccountId::try_from("account-1").unwrap(),
            Some(&engine_core::sync::SyncState::new("old-token")),
        )
        .await
        .unwrap();
    let engine_provider::ContactSourceSync::Available {
        sync,
        cursor_recovered,
    } = result
    else {
        panic!("expected available");
    };
    assert!(cursor_recovered);
    assert_eq!(sync.next_cursor.as_str(), "ctag:ctag-1");
    assert!(matches!(sync.update, SyncUpdate::Snapshot { .. }));
    let reads = replay.reads();
    let (_, _, _, query) = reads.last().unwrap();
    assert!(query.contains("<a:filter/>"), "{query}");
    assert!(!query.contains("prop-filter"), "{query}");
}

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

#[tokio::test]
async fn address_book_discovery_scopes_capabilities_and_rebinding_are_explicit() {
    let replay = Arc::new(Replay::new(vec![ok(PRINCIPAL), ok(BOOKS), ok(BOOKS)]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay), "/.well-known/carddav", "default")
            .await
            .unwrap();
    assert!(matches!(
        provider.address_book_scope(&account()),
        SyncScope::CardDavAddressBookList { .. }
    ));
    assert!(matches!(
        provider.contact_scope(&account()),
        SyncScope::CardDavAddressBook { address_book, .. }
            if address_book.as_str() == "/dav/addressbooks/alice/default/"
    ));
    let destination = provider.contact_destination().unwrap();
    assert_eq!(destination.write_guard, Some(WriteGuard::Enforced));
    assert!(destination.supported_fields.contains(ContactField::Notes));
    assert!(provider.connection_info().capabilities.contact_groups());
    assert!(provider.connection_info().capabilities.contact_photos());
    assert!(format!("{provider:?}").contains("CardDavProvider"));

    let books = provider
        .sync_address_books(&account(), Some(&SyncState::new("ignored")))
        .await
        .unwrap();
    assert!(matches!(
        books,
        ContactSourceSync::Available { sync, .. }
            if matches!(&sync.update, SyncUpdate::Snapshot { objects, .. } if objects.len() == 1)
    ));
    let rebound = provider.rebind("other").unwrap();
    assert!(rebound.contact_destination().is_none());
    assert!(matches!(
        rebound.contact_scope(&account()),
        SyncScope::CardDavAddressBook { address_book, .. }
            if address_book.as_str() == "/dav/addressbooks/alice/other/"
    ));
}

#[tokio::test]
async fn delta_tombstones_and_expired_tokens_report_their_actual_mode() {
    let provider = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![ok(PRINCIPAL), ok(BOOKS), ok(DELTA)])),
        "/.well-known/carddav",
        "default",
    )
    .await
    .unwrap();
    assert!(format!("{provider:?}").contains("CardDavProvider"));
    let result = provider
        .sync_contacts(&account(), Some(&SyncState::new("token-1")))
        .await
        .unwrap();
    assert!(matches!(
        result,
        ContactSourceSync::Available {
            cursor_recovered: false,
            sync,
        } if sync.next_cursor.as_str() == "token-2"
            && matches!(
                &sync.update,
                SyncUpdate::Delta { changed, removed }
                    if changed.len() == 1 && removed.len() == 1
            )
    ));

    let invalid = r#"<D:error xmlns:D="DAV:"><D:valid-sync-token/></D:error>"#;
    let provider = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            ok(BOOKS),
            status(403, invalid),
            ok(CONTACTS),
        ])),
        "/.well-known/carddav",
        "default",
    )
    .await
    .unwrap();
    let recovered = provider
        .sync_contacts(&account(), Some(&SyncState::new("expired")))
        .await
        .unwrap();
    assert!(matches!(
        recovered,
        ContactSourceSync::Available {
            cursor_recovered: true,
            sync,
        } if matches!(sync.update, SyncUpdate::Snapshot { .. })
    ));
}

#[tokio::test]
async fn an_unchanged_ctag_cursor_skips_the_addressbook_query() {
    let replay = Arc::new(Replay::new(vec![ok(PRINCIPAL), ok(BOOKS), ok(CTAG)]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
            .await
            .unwrap();
    let result = provider
        .sync_contacts(&account(), Some(&SyncState::new("ctag:ctag-1")))
        .await
        .unwrap();
    assert!(matches!(
        result,
        ContactSourceSync::Available {
            cursor_recovered: false,
            sync,
        } if matches!(&sync.update, SyncUpdate::Delta { changed, removed }
            if changed.is_empty() && removed.is_empty())
    ));
    let reads = replay.reads();
    assert_eq!(reads.len(), 3);
    assert_eq!(reads.last().unwrap().0, DavMethod::Propfind);
}

#[tokio::test]
async fn direct_fetch_patch_delete_and_photos_use_exact_guards() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        ok(BOOKS),
        ok(CONTACTS),
        wrote(204, Some("\"v2\"")),
        wrote(404, None),
        status(200, "uri-photo"),
    ]));
    let provider =
        CardDavProvider::with_executor(Box::new(replay.clone()), "/.well-known/carddav", "default")
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
    assert_eq!(embedded.as_bytes(), &[1, 2, 3]);
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
    assert_eq!(remote.as_bytes(), b"uri-photo");

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
        Box::new(Replay::new(vec![ok(PRINCIPAL), ok(READ_ONLY_BOOKS)])),
        "/.well-known/carddav",
        "default",
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
            ok(BOOKS),
            wrote(412, None),
        ])),
        "/.well-known/carddav",
        "default",
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
            ok(BOOKS),
            ok("<D:multistatus xmlns:D=\"DAV:\"><D:sync-token>x</D:sync-token></D:multistatus>"),
        ])),
        "/.well-known/carddav",
        "default",
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
