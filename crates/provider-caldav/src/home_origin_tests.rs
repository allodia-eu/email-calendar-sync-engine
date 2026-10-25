//! The principal and the home each keep the host that named them, and the credential reaches a
//! host the account's own server led discovery to only once that host asks for it.
//!
//! iCloud is the case that matters: `caldav.icloud.com` answers the principal and names the
//! calendar home on `pNN-caldav.icloud.com`, and `contacts.icloud.com` does the same for the
//! address-book home. Driven through the live transport against loopback servers, because the
//! `Replay` fake never decides whether a request carries the credential.

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
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

/// A `207` whose one response carries `property` (in `namespace`) naming `href`.
fn naming(property: &str, namespace: &str, href: &str) -> String {
    multistatus(&format!(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:x="{namespace}"><d:response><d:href>/</d:href><d:propstat><d:prop><x:{property}><d:href>{href}</d:href></x:{property}></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    ))
}

const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
const EMPTY: &str = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"/>"#;

fn challenge() -> String {
    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"dav\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        .to_owned()
}

/// Whether each captured request carried the credential, in order.
fn carried(seen: &std::sync::Mutex<Vec<String>>) -> Vec<bool> {
    seen.lock()
        .expect("seen")
        .iter()
        .map(|request| request.to_lowercase().contains("authorization: basic"))
        .collect()
}

#[tokio::test]
async fn a_calendar_home_on_another_host_is_sent_the_credential_once_it_asks() {
    let (home_host, home_seen) = mock_server_capturing(vec![challenge(), multistatus(EMPTY)]);
    let home = format!("{home_host}/123/calendars/");
    let (base, _) = mock_server_capturing(vec![naming("calendar-home-set", CALDAV, &home)]);
    let client = client(&base);

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");
    assert_eq!(found.href, home);
    let path = client.settle_on(&found.href);
    assert_eq!(path, "/123/calendars/");
    client
        .send(DavMethod::Propfind, &path, "1", String::new())
        .await
        .expect("the home is listed");

    assert_eq!(carried(&home_seen), [false, true], "asked, then answered");
}

#[tokio::test]
async fn an_address_book_home_on_another_host_is_sent_the_credential_once_it_asks() {
    let (home_host, home_seen) = mock_server_capturing(vec![challenge(), multistatus(EMPTY)]);
    let home = format!("{home_host}/123/carddavhome/");
    let (base, _) = mock_server_capturing(vec![naming("addressbook-home-set", CARDDAV, &home)]);
    let client = client(&base);

    let found = crate::carddav_ops::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");
    assert_eq!(found.href, home);
    let path = client.settle_on(&found.href);
    client
        .send(DavMethod::Propfind, &path, "1", String::new())
        .await
        .expect("the home is listed");

    assert_eq!(carried(&home_seen), [false, true], "asked, then answered");
}

/// RFC 4918 §8.3 resolves a relative href against the request that returned it, so a principal
/// the configured server names as `/123/principal/` is on the configured server, and stays
/// there after the connection settles on a home elsewhere.
#[tokio::test]
async fn a_principal_stays_on_the_host_that_named_it() {
    let (home_host, home_seen) = mock_server_capturing(vec![]);
    let home = format!("{home_host}/123/calendars/");
    let (base, base_seen) = mock_server_capturing(vec![
        naming("current-user-principal", "DAV:", "/123/principal/"),
        naming("calendar-home-set", CALDAV, &home),
        multistatus(EMPTY),
    ]);
    let client = client(&base);

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");
    client.settle_on(&found.href);
    let principal = found.principal.expect("a principal");
    assert_eq!(principal, format!("{base}/123/principal/"));
    crate::discovery::principal_addresses(&client, &principal)
        .await
        .expect("the principal answers");

    let seen = base_seen.lock().expect("seen");
    assert_eq!(
        seen.len(),
        3,
        "every principal request went to the configured server"
    );
    assert!(
        seen[2]
            .to_lowercase()
            .starts_with("propfind /123/principal/ "),
        "{}",
        seen[2]
    );
    assert!(home_seen.lock().expect("seen").is_empty());
}

/// A principal the configured server names on another host is followed there like a home is:
/// the credential goes once that host asks, and the home it names resolves onto it.
#[tokio::test]
async fn a_principal_on_another_host_is_sent_the_credential_once_it_asks() {
    let (principal_host, principal_seen) = mock_server_capturing(vec![
        challenge(),
        naming("calendar-home-set", CALDAV, "/123/calendars/"),
    ]);
    let principal = format!("{principal_host}/123/principal/");
    let (base, _) =
        mock_server_capturing(vec![naming("current-user-principal", "DAV:", &principal)]);
    let client = client(&base);

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");

    assert_eq!(
        carried(&principal_seen),
        [false, true],
        "asked, then answered"
    );
    assert_eq!(found.href, format!("{principal_host}/123/calendars/"));
}

/// A principal answer that names itself and its home, as a principal resource does.
fn principal_answer(principal: &str, home_property: &str, namespace: &str, home: &str) -> String {
    multistatus(&format!(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:x="{namespace}"><d:response><d:href>{principal}</d:href><d:propstat><d:prop><d:current-user-principal><d:href>{principal}</d:href></d:current-user-principal><x:{home_property}><d:href>{home}</d:href></x:{home_property}></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    ))
}

/// A host that stored the principal starts the next connect there (RFC 6764 §6): the walk asks
/// it for the home, sends the credential only once it asks, and reports the principal back.
#[tokio::test]
async fn a_connect_resumes_at_a_stored_principal() {
    let (host, seen) = mock_server_capturing(vec![
        challenge(),
        principal_answer("/123/principal/", "calendar-home-set", CALDAV, "/123/calendars/"),
        "HTTP/1.1 200 OK\r\nDAV: 1, calendar-access\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_owned(),
    ]);
    let principal = format!("{host}/123/principal/");
    let provider = crate::CalDavProvider::connect(
        crate::CalDavConfig::new(
            host.clone(),
            Credentials::Basic {
                username: "alice".to_owned(),
                password: "super-secret".to_owned(),
            },
            engine_http::test_server::user_agent(),
        )
        .with_discovery_path(principal.clone()),
    )
    .await
    .expect("the stored principal answers");

    assert_eq!(provider.principal_url(), Some(principal.as_str()));
    let seen = seen.lock().expect("seen");
    assert!(
        seen[0]
            .to_lowercase()
            .starts_with("propfind /123/principal/ "),
        "{}",
        seen[0]
    );
    assert!(
        !seen[0].to_lowercase().contains("authorization"),
        "nothing sent unasked"
    );
    assert!(seen[1].to_lowercase().contains("authorization: basic"));
}

#[tokio::test]
async fn the_carddav_walk_reports_the_principal_it_found() {
    let (base, _) = mock_server_capturing(vec![
        naming("current-user-principal", "DAV:", "/123/principal/"),
        principal_answer(
            "/123/principal/",
            "addressbook-home-set",
            CARDDAV,
            "/123/carddavhome/",
        ),
    ]);
    let client = client(&base);

    let found = crate::carddav_ops::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");

    assert_eq!(found.principal, Some(format!("{base}/123/principal/")));
    assert_eq!(found.href, format!("{base}/123/carddavhome/"));
}
