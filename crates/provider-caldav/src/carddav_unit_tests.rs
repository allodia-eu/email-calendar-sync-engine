use engine_core::{
    contact::{ContactCard, ContactEmail, ContactName, ContactProperty, PropertyId},
    ids::{AddressBookId, ContactId},
    membership::Memberships,
};
use engine_provider::{ContactsProvider, Provider};
use engine_tls::TlsClientConfig;

use crate::{
    CardDavConfig, CardDavProvider, Credentials,
    carddav_ops::{
        bind_collection, decode_data_uri, discover_home, encode_segment, multiget_report,
        stable_suffix,
    },
    test_support::{Replay, ok},
    transport::HttpResponse,
};

fn credentials() -> Credentials {
    Credentials::Basic {
        username: "alice".into(),
        password: "secret".into(),
    }
}

#[tokio::test]
async fn public_config_builders_and_invalid_connect_are_explicit() {
    let config = CardDavConfig::new("not a url", credentials())
        .with_address_book("team")
        .with_tls(TlsClientConfig::default());
    assert_eq!(config.base_url, "not a url");
    assert_eq!(config.discovery_path, "/.well-known/carddav");
    assert_eq!(config.address_book, "team");
    assert!(CardDavProvider::connect(config).await.is_err());
}

#[test]
fn collection_uri_and_stable_resource_helpers_cover_edge_input() {
    assert_eq!(
        bind_collection("/dav/books/alice/", "team")
            .unwrap()
            .as_str(),
        "/dav/books/alice/team/"
    );
    assert_eq!(
        bind_collection("/ignored/", "/dav/books/alice/team/")
            .unwrap()
            .as_str(),
        "/dav/books/alice/team/"
    );
    assert_eq!(encode_segment("Ada + Zoë"), "Ada%20%2B%20Zo%C3%AB");
    let report = multiget_report("/book/a&b.vcf");
    assert!(report.contains("/book/a&amp;b.vcf"));

    let book = AddressBookId::try_from("/book/").unwrap();
    let mut card = ContactCard::new(
        ContactId::try_from("/book/ada.vcf").unwrap(),
        Memberships::of_one(book),
    );
    card.name = Some(ContactName {
        full: Some("Ada".into()),
        ..ContactName::default()
    });
    card.emails.insert(
        PropertyId::new("email").unwrap(),
        ContactProperty::new(ContactEmail::new("ada@example.test")),
    );
    assert!(stable_suffix(&card).starts_with("contact-"));

    assert_eq!(decode_data_uri("image/png;base64,AQID").unwrap(), [1, 2, 3]);
    assert!(decode_data_uri("image/png;base64").is_err());
    assert!(decode_data_uri("image/png;base64,?").is_err());
}

#[tokio::test]
async fn discovery_accepts_direct_home_and_fails_closed_on_missing_or_redirected_data() {
    let direct = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/</D:href><D:propstat><D:prop><C:addressbook-home-set><D:href>/books/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
    assert_eq!(
        discover_home(&Replay::new(vec![ok(direct)]), "/start")
            .await
            .unwrap(),
        "/books/"
    );

    let empty = r#"<D:multistatus xmlns:D="DAV:"/>"#;
    assert!(
        discover_home(&Replay::new(vec![ok(empty)]), "/start")
            .await
            .is_err()
    );
    let principal = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principal/</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
    assert!(
        discover_home(&Replay::new(vec![ok(principal), ok(empty)]), "/start",)
            .await
            .is_err()
    );

    let redirects = (0..4)
        .map(|index| HttpResponse {
            status: 307,
            body: String::new(),
            location: Some(format!("/redirect-{index}")),
            etag: None,
        })
        .collect();
    assert!(
        discover_home(&Replay::new(redirects), "/start")
            .await
            .is_err()
    );
}

const HOME: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/</D:href><D:propstat><D:prop><C:addressbook-home-set><D:href>/dav/addressbooks/alice/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// Two books: `default` grants the `DAV:all` aggregate, `shared` grants only `read`.
const MIXED_BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Contacts</D:displayname><D:current-user-privilege-set><D:privilege><D:all/></D:privilege><D:privilege><D:read/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/dav/addressbooks/alice/shared/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Shared</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// `DAV:all` is the RFC 3744 aggregate *above* `DAV:write`, so a server that reports it
/// alongside `read` has granted writes. Reading it as read-only made every write fail
/// against a book the user owns — and the calendar path already got this right, which
/// is why both now share one predicate.
///
/// The same fixture pins the rebind contract: switching to another book in the home
/// re-derives write capability from the privileges discovery already collected, rather
/// than assuming the worst and stranding the caller with a read-only provider.
#[tokio::test]
async fn write_capability_follows_the_aggregate_privilege_and_survives_rebinding() {
    let provider = CardDavProvider::with_executor(
        Box::new(Replay::new(vec![ok(HOME), ok(MIXED_BOOKS)])),
        "/.well-known/carddav",
        "default",
    )
    .await
    .unwrap();
    assert!(provider.connection_info().capabilities.contact_writes());
    assert!(provider.contact_destination().is_some_and(|d| d.writable));

    let shared = provider.rebind("shared").unwrap();
    assert!(!shared.connection_info().capabilities.contact_writes());
    assert!(shared.contact_destination().is_none());

    // ...and back onto the writable book, without a second round of discovery.
    let back = shared.rebind("default").unwrap();
    assert!(back.connection_info().capabilities.contact_writes());
    assert!(back.contact_destination().is_some_and(|d| d.writable));
}
