use std::sync::Arc;

use engine_core::{
    contact::{ContactCard, ContactDraft, ContactField},
    ids::{AccountId, AddressBookId, ContactId},
    membership::Memberships,
    sync::{SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{
    ContactSourceSync, ContactsProvider, IgnoreConnectSteps, Provider, WriteGuard,
};

use crate::{
    CardDavProvider,
    test_support::{Replay, ok, options, status, wrote},
    transport::{DavMethod, Precondition},
};

pub(crate) const PRINCIPAL: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/</D:href><D:propstat><D:prop><C:addressbook-home-set><D:href>/dav/addressbooks/alice/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
pub(crate) const BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Contacts</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege><D:privilege><D:write-content/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
pub(crate) const CONTACTS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/ada.vcf</D:href><D:propstat><D:prop><D:getetag>"v1"</D:getetag><C:address-data><![CDATA[BEGIN:VCARD
VERSION:4.0
UID:ada
FN:Ada Lovelace
EMAIL;TYPE=work:Ada@Example.COM
X-KEEP:untouched
END:VCARD
]]></C:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:sync-token>token-1</D:sync-token></D:multistatus>"#;
const CTAG: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><CS:getctag>ctag-1</CS:getctag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
pub(crate) const READ_ONLY_BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Read only</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
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
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(CONTACTS_ONE_UNPARSEABLE),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(CONTACTS),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        wrote(201, Some("\"v1\"")),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        status(405, "sync-collection unsupported"),
        ok(CTAG),
        ok(CONTACTS),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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

pub(crate) fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

#[tokio::test]
async fn address_book_discovery_scopes_capabilities_and_rebinding_are_explicit() {
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(BOOKS),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
            ok(DELTA),
        ])),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
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
                SyncUpdate::Delta { changed, removed, .. }
                    if changed.len() == 1 && removed.len() == 1
            )
    ));

    let invalid = r#"<D:error xmlns:D="DAV:"><D:valid-sync-token/></D:error>"#;
    let provider = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            ok(PRINCIPAL),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
            status(403, invalid),
            ok(CONTACTS),
        ])),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
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
    let replay = Arc::new(Replay::new(vec![
        ok(PRINCIPAL),
        options(Some("1, 3, addressbook")),
        ok(BOOKS),
        ok(CTAG),
    ]));
    let provider = CardDavProvider::with_executor(
        Box::new(replay.clone()),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
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
        } if matches!(&sync.update, SyncUpdate::Delta { changed, removed, .. }
            if changed.is_empty() && removed.is_empty())
    ));
    // Discovery, the home's `OPTIONS`, the address-book list, then the one `PROPFIND` of the
    // ctag: no query for cards.
    let reads = replay.reads();
    assert_eq!(reads.len(), 4);
    assert_eq!(reads.last().unwrap().0, DavMethod::Propfind);
}
