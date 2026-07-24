use engine_core::{
    contact::{ContactCard, ContactEmail, ContactName, ContactProperty, PropertyId},
    ids::{AddressBookId, ContactId},
    membership::Memberships,
};
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
