//! Whether a CardDAV account has address books is the server's answer, and a connect is traced.
//!
//! RFC 6352 §6.1: a server supporting CardDAV advertises the `addressbook` class in the `DAV:`
//! header of an `OPTIONS` response. A server that does not is one with no contacts for this
//! account, which is a capability the host reads, not a failure.

use std::sync::{Arc, Mutex};

use engine_core::ids::AccountId;
use engine_provider::{
    ConnectObserver, ConnectStep, ContactSourceSync, ContactsProvider, Provider,
};

use crate::{
    CardDavConfig, CardDavProvider,
    test_support::{Replay, mock_server, multistatus, ok, options, options_response, redirect},
    transport::{Credentials, DavMethod, HttpResponse},
};

const HOME: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/</D:href><D:propstat><D:prop><C:addressbook-home-set><D:href>/dav/addressbooks/alice/</D:href></C:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
const BOOKS: &str = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:response><D:href>/dav/addressbooks/alice/default/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:addressbook/></D:resourcetype><D:displayname>Contacts</D:displayname><D:current-user-privilege-set><D:privilege><D:read/></D:privilege><D:privilege><D:write-content/></D:privilege></D:current-user-privilege-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// The `DAV:` header Stalwart really returns, which lists `addressbook`.
const STALWART_DAV: &str = include_str!("../tests/fixtures/options-dav-stalwart.txt");
/// The header from the SabreDAV fixture, which serves calendars only.
const SABREDAV_DAV: &str = include_str!("../tests/fixtures/options-dav-sabredav.txt");

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

async fn connect(responses: Vec<HttpResponse>) -> (CardDavProvider, Arc<Replay>) {
    let replay = Arc::new(Replay::new(responses));
    let provider = CardDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/carddav",
        "default",
        &Steps::default(),
    )
    .await
    .unwrap();
    (provider, replay)
}

#[tokio::test]
async fn a_server_advertising_the_addressbook_class_offers_contacts() {
    let (provider, replay) = connect(vec![
        ok(HOME),
        options(Some(STALWART_DAV.trim())),
        ok(BOOKS),
    ])
    .await;

    assert!(provider.connection_info().capabilities.contacts());
    assert!(provider.contact_destination().is_some());
    // The header belongs to a DAV resource, so it is asked of the address-book home, not of
    // the connection's root.
    assert!(
        replay
            .seen()
            .iter()
            .any(|(method, href)| *method == DavMethod::Options
                && href == "/dav/addressbooks/alice/"),
        "{:?}",
        replay.seen()
    );
}

#[tokio::test]
async fn a_server_without_the_addressbook_class_offers_no_contacts() {
    let (provider, replay) = connect(vec![ok(HOME), options(Some(SABREDAV_DAV.trim()))]).await;

    let capabilities = provider.connection_info().capabilities;
    assert!(!capabilities.contacts());
    assert!(!capabilities.contact_writes());
    assert!(!capabilities.contact_photos());
    assert!(provider.contact_destination().is_none());
    // Nothing to list, so nothing is spent listing it.
    assert_eq!(replay.seen().len(), 2, "{:?}", replay.seen());

    // A pass reads as unavailable rather than failing, so sibling sources carry on.
    assert!(matches!(
        provider.sync_address_books(&account(), None).await.unwrap(),
        ContactSourceSync::Unavailable(_)
    ));
    assert!(matches!(
        provider.sync_contacts(&account(), None).await.unwrap(),
        ContactSourceSync::Unavailable(_)
    ));
}

#[tokio::test]
async fn an_options_request_the_server_refuses_reads_as_no_address_books() {
    // A `405` carries no `DAV:` header; refusing the whole connect over it would turn a
    // server that has nothing to offer into an error.
    let refused = HttpResponse {
        status: 405,
        body: String::new(),
        location: None,
        etag: None,
        dav: None,
        retry_after: None,
    };
    let (provider, _) = connect(vec![ok(HOME), refused]).await;
    assert!(!provider.connection_info().capabilities.contacts());
}

#[tokio::test]
async fn a_rebound_provider_keeps_the_servers_answer() {
    let (provider, _) = connect(vec![ok(HOME), options(Some(SABREDAV_DAV.trim()))]).await;
    let rebound = provider.rebind("other").unwrap();
    assert!(!rebound.connection_info().capabilities.contacts());
}

/// Records the steps an observer sees, one line each.
#[derive(Default)]
struct Steps(Mutex<Vec<String>>);

impl ConnectObserver for Steps {
    fn step(&self, step: &ConnectStep<'_>) {
        let line = match step {
            ConnectStep::Redirected { from, to, .. } => format!("redirected {from} -> {to}"),
            ConnectStep::Discovered { endpoint, .. } => format!("discovered {endpoint}"),
            ConnectStep::Negotiated {
                dialect, features, ..
            } => format!("negotiated {dialect} [{}]", features.join(", ")),
            other => format!("{other:?}"),
        };
        self.0.lock().unwrap().push(line);
    }
}

#[tokio::test]
async fn a_connect_reports_each_redirect_the_home_and_what_the_server_advertised() {
    let moved = HttpResponse {
        status: 301,
        body: String::new(),
        location: Some("/dav/".to_owned()),
        etag: None,
        dav: None,
        retry_after: None,
    };
    let steps = Steps::default();
    CardDavProvider::with_executor(
        Box::new(Replay::new(vec![
            moved,
            ok(HOME),
            options(Some("1, 3, addressbook")),
            ok(BOOKS),
        ])),
        "/.well-known/carddav",
        "default",
        &steps,
    )
    .await
    .unwrap();

    assert_eq!(
        *steps.0.lock().unwrap(),
        [
            "redirected /.well-known/carddav -> /dav/",
            "discovered /dav/addressbooks/alice/",
            "negotiated CardDAV [1, 3, addressbook]",
        ]
    );
}

/// The same trace through what a host actually drives: the config carries the observer, and
/// `connect` dials the real HTTP client.
#[tokio::test]
async fn a_connect_from_the_config_carries_its_observer_over_the_wire() {
    let base = mock_server(vec![
        redirect("/dav/"),
        multistatus(HOME),
        options_response("1, 3, addressbook"),
        multistatus(BOOKS),
    ]);
    let steps = Arc::new(Steps::default());
    let provider = CardDavProvider::connect(
        CardDavConfig::new(base, alice("pw"))
            .with_retry(engine_http::RetryConfig::default())
            .with_connect_observer(Arc::clone(&steps) as Arc<dyn ConnectObserver>),
    )
    .await
    .unwrap();

    assert!(provider.connection_info().capabilities.contacts());
    assert_eq!(
        *steps.0.lock().unwrap(),
        [
            "redirected /.well-known/carddav -> /dav/",
            "discovered /dav/addressbooks/alice/",
            "negotiated CardDAV [1, 3, addressbook]",
        ]
    );
}

#[test]
fn config_debug_shows_the_observer_without_leaking_the_password() {
    // Hand-written because a `dyn` observer is not `Debug`, so the redaction the derive
    // inherited from `Credentials` is asserted here.
    let config = CardDavConfig::new("https://dav.example.com", alice("hunter2"));
    let shown = format!("{config:?}");
    assert!(shown.contains("alice") && shown.contains("dav.example.com"));
    assert!(
        !shown.contains("hunter2"),
        "password must not leak: {shown}"
    );
    assert!(shown.contains("connect_observer: false"), "{shown}");

    let observed = config.with_connect_observer(Arc::new(Steps::default()));
    assert!(format!("{observed:?}").contains("connect_observer: true"));
}

fn alice(password: &str) -> Credentials {
    Credentials::Basic {
        username: "alice".to_owned(),
        password: password.to_owned(),
    }
}
