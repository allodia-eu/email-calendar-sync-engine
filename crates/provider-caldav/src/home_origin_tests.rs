//! A home the account's own server names on another host is where the account's collections
//! are, so the credential follows it there, as it follows a discovery redirect.
//!
//! iCloud is the case that matters: `caldav.icloud.com` answers the principal and names the
//! calendar home on `pNN-caldav.icloud.com`, and `contacts.icloud.com` does the same for the
//! address-book home. Driven through the live transport against two loopback servers, because
//! the `Replay` fake never decides whether a request carries the credential.

use engine_provider::IgnoreConnectSteps;

use crate::{
    test_support::{mock_server_capturing, multistatus},
    transport::{Credentials, DavClient, DavExecutor, DavMethod},
};

fn client(base: &str) -> DavClient {
    DavClient::new(
        base,
        Credentials::Basic {
            username: "alice".to_owned(),
            password: "super-secret".to_owned(),
        },
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

fn principal_naming(property: &str, namespace: &str, home: &str) -> String {
    multistatus(&format!(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:x="{namespace}"><d:response><d:href>/123/principal/</d:href><d:propstat><d:prop><x:{property}><d:href>{home}</d:href></x:{property}></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    ))
}

const EMPTY: &str = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;

#[tokio::test]
async fn the_credential_follows_a_calendar_home_on_another_host() {
    let (home_host, home_seen) = mock_server_capturing(vec![multistatus(EMPTY)]);
    let home = format!("{home_host}/123/calendars/");
    let (base, _) = mock_server_capturing(vec![principal_naming(
        "calendar-home-set",
        "urn:ietf:params:xml:ns:caldav",
        &home,
    )]);
    let client = client(&base);

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");
    assert_eq!(found.href, home);
    client
        .send(DavMethod::Propfind, &found.href, "1", String::new())
        .await
        .expect("the home is listed");

    let sent = home_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(
        sent.contains("authorization: basic"),
        "the calendar home was listed without the credential: {sent}"
    );
}

#[tokio::test]
async fn the_credential_follows_an_address_book_home_on_another_host() {
    let (home_host, home_seen) = mock_server_capturing(vec![multistatus(EMPTY)]);
    let home = format!("{home_host}/123/carddavhome/");
    let (base, _) = mock_server_capturing(vec![principal_naming(
        "addressbook-home-set",
        "urn:ietf:params:xml:ns:carddav",
        &home,
    )]);
    let client = client(&base);

    let found = crate::carddav_ops::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");
    assert_eq!(found, home);
    client
        .send(DavMethod::Propfind, &found, "1", String::new())
        .await
        .expect("the home is listed");

    let sent = home_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(
        sent.contains("authorization: basic"),
        "the address-book home was listed without the credential: {sent}"
    );
}
