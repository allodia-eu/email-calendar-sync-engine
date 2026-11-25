//! Which origins the live transport trusts with the credential, and when: an origin the
//! account's own server led discovery to only once it asks, a content URL never, a URL that
//! names no origin moves nothing, and a connection on TLS never trusts plaintext. Split from
//! `transport_tests.rs` by responsibility.

use super::{http_transport_tests::challenge, *};
use crate::{Credentials, test_support::mock_server_capturing};

fn ok_body() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi".to_owned()
}

fn basic_client(base: &str) -> DavClient {
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

/// The counterpart of the foreign-photo rule: an origin the account's own server led
/// discovery to may receive the credential, but only once it asks for it (RFC 9110 §11.5:
/// a protection space does not extend past its own server), and from then on unasked.
#[tokio::test]
async fn a_trusted_origin_is_sent_the_credential_only_once_it_asks() {
    let (moved, moved_seen) = mock_server_capturing(vec![challenge(), ok_body(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);

    assert!(
        client
            .trust_origin(&format!("{moved}/principals/u/"))
            .is_ok()
    );
    let photo = format!("{moved}/calendars/u/a.png");
    DavExecutor::get_bytes(&client, &photo)
        .await
        .expect("answered after the challenge");
    DavExecutor::get_bytes(&client, &photo)
        .await
        .expect("answered again");

    let seen = moved_seen.lock().expect("seen");
    let carried: Vec<bool> = seen
        .iter()
        .map(|request| request.to_lowercase().contains("authorization: basic"))
        .collect();
    assert_eq!(
        carried,
        [false, true, true],
        "unasked, answered, then sent: {seen:?}"
    );
}

/// A foreign host that *asks* for the credential still does not get it: only an origin the
/// account's server led discovery to may answer a challenge.
#[tokio::test]
async fn a_foreign_photo_host_that_asks_for_the_credential_is_not_sent_it() {
    let (foreign, foreign_seen) = mock_server_capturing(vec![challenge(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);

    let err = DavExecutor::get_bytes(&client, &format!("{foreign}/p.png"))
        .await
        .expect_err("the challenge stands");
    assert!(err.to_string().contains("401"), "{err}");
    let seen = foreign_seen.lock().expect("seen");
    assert_eq!(seen.len(), 1, "asked once, never again with the password");
    assert!(!seen[0].to_lowercase().contains("authorization"));
}

/// Settling on the home moves where relative hrefs resolve: every href a response from the
/// home carries is relative to the home's origin (RFC 4918 §8.3).
#[tokio::test]
async fn settling_on_a_home_resolves_relative_hrefs_onto_its_origin() {
    let (moved, moved_seen) = mock_server_capturing(vec![challenge(), ok_body()]);
    let (base, _) = mock_server_capturing(vec![]);
    let client = basic_client(&base);
    let home = format!("{moved}/calendars/u/");

    assert!(client.trust_origin(&home).is_ok());
    assert_eq!(client.settle_on(&home), "/calendars/u/");
    DavExecutor::get_bytes(&client, "/calendars/u/a.png")
        .await
        .expect("get after settling");

    let sent = moved_seen.lock().expect("seen").join("\n").to_lowercase();
    assert!(sent.contains("get /calendars/u/a.png "), "{sent}");
    assert!(sent.contains("authorization: basic"), "{sent}");
}

/// Only a resolvable, origin-bearing URL names anything to trust or settle on: a href a
/// server merely mentions moves nothing.
#[tokio::test]
async fn a_url_that_names_no_origin_trusts_and_moves_nothing() {
    let (base, _) = mock_server_capturing(vec![]);
    let client = DavClient::new(
        &base,
        Credentials::Bearer("t".to_owned()),
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let before = format!("{client:?}");
    for noise in ["/principals/u/", "not a url", "data:text/plain,hi", ""] {
        assert!(client.trust_origin(noise).is_ok(), "refused {noise:?}");
        assert_eq!(client.settle_on(noise), noise);
        assert_eq!(format!("{client:?}"), before, "moved by {noise:?}");
    }
}

/// An origin the server leads discovery to is trusted only while it stays on TLS.
/// `href::redirect_href` cannot catch this hop, since discovery starts at a bare well-known
/// path that names no scheme, so the connection is what refuses, and the walk fails rather
/// than putting the account's password on the wire in the clear.
#[tokio::test]
async fn an_origin_that_leaves_tls_is_not_trusted() {
    let client = basic_client("https://dav.example.com");
    let plaintext = client.trust_origin("http://evil.test/principals/u/");
    let insecure = Some(engine_provider::CredentialFailureKind::Insecure);
    assert_eq!(
        plaintext.unwrap_err().into_error().credential_failure(),
        insecure
    );
    assert!(
        client
            .trust_origin("https://dav.example.net/principals/u/")
            .is_ok()
    );
}

/// A server that answers `401` with no challenge at all (Yahoo's calendar server does, against
/// RFC 9110 §15.5.2) is still asking for credentials: a trusted origin is sent them.
#[tokio::test]
async fn a_bare_401_from_a_trusted_origin_is_answered() {
    let unasked =
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
    let (base, seen) = mock_server_capturing(vec![unasked, ok_body()]);
    let client = basic_client(&base);

    DavExecutor::get_bytes(&client, "/principals/a.png")
        .await
        .expect("answered after the bare 401");

    let seen = seen.lock().expect("seen");
    assert!(!seen[0].to_lowercase().contains("authorization"));
    assert!(seen[1].to_lowercase().contains("authorization: basic"));
}
