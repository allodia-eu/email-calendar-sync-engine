//! Tests for how a connection authenticates: the challenge reading, and an OAuth token
//! following what the server's challenge asks for over the live transport.

use super::*;
use crate::{
    test_support::{mock_server_capturing, multistatus},
    transport::{DavClient, DavExecutor, DavMethod},
};

#[test]
fn a_challenge_names_its_schemes_and_not_its_parameters() {
    // sabre/dav's own challenge: `charset` is a parameter of Basic, not a second scheme.
    assert_eq!(
        challenge_schemes(r#"Basic realm="sabre/dav", charset="UTF-8""#),
        ["basic"]
    );
    assert_eq!(
        challenge_schemes(r#"Bearer realm="dav", error="invalid_token""#),
        ["bearer"]
    );
    // RFC 9110 §11.6.1: several challenges in one field, a comma inside a quoted string,
    // a token68 and whitespace around `=`.
    assert_eq!(
        challenge_schemes(r#"Negotiate YII=, Basic realm = "a, b", Bearer"#),
        ["negotiate", "basic", "bearer"]
    );
    assert!(challenge_schemes("").is_empty());
}

fn token() -> Credentials {
    Credentials::Token {
        username: "alice@example.test".to_owned(),
        token: "access-token".to_owned(),
    }
}

/// A `401` whose challenge is `challenge`.
fn refused(challenge: &str) -> String {
    format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn client(base: &str, credentials: Credentials) -> DavClient {
    DavClient::new(
        base,
        credentials,
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client")
}

async fn propfind(client: &DavClient) -> u16 {
    DavExecutor::send(client, DavMethod::Propfind, "/dav/", "0", String::new())
        .await
        .expect("propfind")
        .status
}

/// The `Authorization` header of a captured request head, lowercased.
fn authorization(head: &str) -> String {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_token_refused_by_a_basic_only_challenge_is_presented_as_the_basic_password() {
    // Observed on a sabre/dav deployment behind an OAuth mail provider: Bearer is refused
    // with `WWW-Authenticate: Basic`, and the same access token is accepted as the Basic
    // password for the account's address.
    let (base, seen) = mock_server_capturing(vec![
        refused(r#"Basic realm="sabre/dav", charset="UTF-8""#),
        multistatus("<d:multistatus xmlns:d=\"DAV:\"/>"),
        multistatus("<d:multistatus xmlns:d=\"DAV:\"/>"),
    ]);
    let client = client(&base, token());

    assert_eq!(propfind(&client).await, 207);
    assert_eq!(propfind(&client).await, 207);

    let seen = seen.lock().expect("seen");
    // `alice@example.test:access-token`, as RFC 7617 encodes it.
    let basic = "Basic YWxpY2VAZXhhbXBsZS50ZXN0OmFjY2Vzcy10b2tlbg==";
    assert_eq!(authorization(&seen[0]), "Bearer access-token");
    assert_eq!(
        authorization(&seen[1]),
        basic,
        "the refused request is sent again"
    );
    assert_eq!(
        authorization(&seen[2]),
        basic,
        "the connection keeps what it learnt rather than being refused once per request"
    );
}

#[tokio::test]
async fn a_token_refused_by_a_bearer_challenge_is_not_sent_again() {
    // A server that takes Bearer and refused this one has said the token is bad, not that
    // it is presented wrongly. Sending it as a password would only add a failed login.
    let (base, seen) = mock_server_capturing(vec![refused(
        r#"Bearer realm="dav", error="invalid_token""#,
    )]);
    assert_eq!(propfind(&client(&base, token())).await, 401);
    assert_eq!(seen.lock().expect("seen").len(), 1);
}

#[tokio::test]
async fn a_server_offering_both_schemes_keeps_getting_bearer() {
    let (base, seen) = mock_server_capturing(vec![refused(r#"Basic realm="dav", Bearer"#)]);
    assert_eq!(propfind(&client(&base, token())).await, 401);
    assert_eq!(seen.lock().expect("seen").len(), 1);
}

#[tokio::test]
async fn a_bare_bearer_never_becomes_a_basic_password() {
    // `Credentials::Bearer` names no account to pair the token with, so there is no Basic
    // form of it to try.
    let (base, seen) = mock_server_capturing(vec![refused(r#"Basic realm="dav""#)]);
    let client = client(&base, Credentials::Bearer("access-token".to_owned()));
    assert_eq!(propfind(&client).await, 401);
    assert_eq!(seen.lock().expect("seen").len(), 1);
}

#[test]
fn a_token_is_redacted_in_debug_output() {
    let shown = format!("{:?}", token());
    assert!(shown.contains("alice@example.test"), "{shown}");
    assert!(!shown.contains("access-token"), "{shown}");
}
