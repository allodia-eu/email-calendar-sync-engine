//! How each way a server asks for the credential is answered, through the live transport.
//!
//! Servers ask in three ways, and each is a different code path:
//! - a `401` with a `WWW-Authenticate` challenge (Stalwart, iCloud, Fastmail, SabreDAV's default);
//! - a `401` with no challenge at all, against RFC 9110 §15.5.2 (Yahoo, Google, AOL);
//! - a `207` that lets the anonymous request through and names the requester
//!   `<DAV:unauthenticated/>` (RFC 5397 §3; SabreDAV when anonymous reads are allowed).
//!
//! `docker/sabredav` serves the second and third for real (`/bare/` and `/open/`), and
//! `tests/live_challenges.rs` drives them; these pin the same rules without a container.

use std::sync::{Arc, Mutex};

use engine_provider::IgnoreConnectSteps;

use crate::{
    CalDavError, CredentialHosts,
    test_support::{mock_server_capturing, multistatus, options_response},
    transport::{Credentials, DavClient, DavExecutor},
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

fn status(line: &str, headers: &str) -> String {
    format!("HTTP/1.1 {line}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n")
}

const UNAUTHENTICATED: &str = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:unauthenticated/></d:current-user-principal></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;

fn principal_and_home(home: &str) -> String {
    multistatus(&format!(
        r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/p/</d:href></d:current-user-principal><c:calendar-home-set><d:href>{home}</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#
    ))
}

/// Whether each captured request carried the credential, in order.
fn carried(seen: &Mutex<Vec<String>>) -> Vec<bool> {
    seen.lock()
        .expect("seen")
        .iter()
        .map(|request| request.to_lowercase().contains("authorization: basic"))
        .collect()
}

#[tokio::test]
async fn a_start_that_lets_an_anonymous_request_through_is_asked_again_with_the_credential() {
    let (base, seen) = mock_server_capturing(vec![
        multistatus(UNAUTHENTICATED),
        principal_and_home("/home/"),
    ]);
    let client = client(&base);

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found once the credential is sent");

    assert_eq!(found.href, format!("{base}/home/"));
    assert_eq!(
        carried(&seen),
        [false, true],
        "anonymous, then authenticated"
    );
}

#[tokio::test]
async fn a_bare_401_at_the_start_is_answered() {
    let (base, seen) = mock_server_capturing(vec![
        status("401 Unauthorized", ""),
        principal_and_home("/home/"),
    ]);
    let client = client(&base);

    crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("answered");

    assert_eq!(carried(&seen), [false, true]);
}

#[tokio::test]
async fn a_server_that_asks_only_for_another_method_is_not_a_refused_sign_in() {
    let (base, seen) = mock_server_capturing(vec![status(
        "401 Unauthorized",
        "WWW-Authenticate: Digest realm=\"dav\", qop=\"auth\", nonce=\"n\"\r\n",
    )]);
    let client = client(&base);

    let err = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect_err("nothing this credential can answer");

    assert!(
        matches!(&err, CalDavError::UnsupportedAuthentication(offered) if offered == "Digest"),
        "{err:?}"
    );
    assert_ne!(
        err.failure_class(),
        engine_core::error::FailureClass::Authentication,
        "the password was never sent, so it was not refused"
    );
    assert_eq!(carried(&seen), [false], "and it was not sent");
}

/// The DAV classes a home's `OPTIONS` reports are read only once its origin has the credential,
/// whichever way it asks: a challenge, or a request let through anonymously.
#[tokio::test]
async fn a_new_home_host_is_authenticated_before_its_classes_are_read() {
    for (name, probe) in [
        (
            "a challenge",
            vec![
                status(
                    "401 Unauthorized",
                    "WWW-Authenticate: Basic realm=\"dav\"\r\n",
                ),
                multistatus(r#"<d:multistatus xmlns:d="DAV:"/>"#),
            ],
        ),
        (
            "an anonymous request let through",
            vec![multistatus(UNAUTHENTICATED)],
        ),
    ] {
        let mut answers = probe;
        answers.push(options_response("1, 3, addressbook"));
        let (home_host, home_seen) = mock_server_capturing(answers);
        let (base, _) = mock_server_capturing(vec![]);
        let client = client(&base);
        let home = format!("{home_host}/books/");
        assert!(client.trust_origin(&home));

        let classes = crate::carddav_ops::advertised_classes(&client, &home)
            .await
            .expect("classes");

        assert!(classes.iter().any(|class| class == "addressbook"), "{name}");
        let options = home_seen
            .lock()
            .expect("seen")
            .last()
            .cloned()
            .unwrap_or_default();
        assert!(options.starts_with("OPTIONS "), "{name}: {options}");
        assert!(
            options.to_lowercase().contains("authorization: basic"),
            "{name}: the classes were read anonymously"
        );
    }
}

/// Permits exactly one host.
struct Only(&'static str);

impl CredentialHosts for Only {
    fn permits(&self, host: &str) -> bool {
        host == self.0
    }
}

#[tokio::test]
async fn a_principal_the_credential_may_not_reach_is_dropped_once_the_home_is_known() {
    // `localhost` and `127.0.0.1` are the same machine and different hosts, which is all a
    // host-name policy can see.
    let answer = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>http://localhost:1/p/</d:href></d:current-user-principal><c:calendar-home-set><d:href>/home/</d:href></c:calendar-home-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>"#;
    let (base, _) = mock_server_capturing(vec![multistatus(answer)]);
    let client = client(&base).with_credential_hosts(Some(Arc::new(Only("127.0.0.1"))));

    let found = crate::discovery::discover_home(&client, "/", &IgnoreConnectSteps)
        .await
        .expect("the home is found");

    assert_eq!(found.href, format!("{base}/home/"));
    assert_eq!(found.principal, None, "the refused principal is not kept");
}
