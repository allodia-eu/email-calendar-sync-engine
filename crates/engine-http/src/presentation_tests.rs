//! How each kind of credential goes on a request, which kinds a token-only API refuses before
//! anything is sent, and what a `401`'s challenge says about the credential it refused.

use engine_core::error::FailureClass;
use engine_provider::{Credential, CredentialFailure};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, WWW_AUTHENTICATE};

use super::{judge_challenge, present_bearer, present_credential};
use crate::Unauthorized;

fn administrator() -> Credential {
    Credential::acting_as("admin", "secret", "alice")
}

/// The `Authorization` a presentation put on a request, or the failure that stopped it.
fn authorization(
    presented: Result<reqwest::RequestBuilder, CredentialFailure>,
) -> Result<String, FailureClass> {
    let request = presented
        .map_err(|failure| failure.into_error().class())?
        .build()
        .expect("a request");
    Ok(request
        .headers()
        .get(AUTHORIZATION)
        .expect("an authorization header")
        .to_str()
        .expect("ascii")
        .to_owned())
}

fn request() -> reqwest::RequestBuilder {
    crate::client(
        &engine_tls::TlsClientConfig::bundled(),
        &crate::test_server::user_agent(),
    )
    .build()
    .expect("client")
    .get("https://dav.example.com/")
}

#[test]
fn a_password_goes_as_basic_and_a_token_as_bearer() {
    assert_eq!(
        authorization(present_credential(
            request(),
            &Credential::password("alice", "old")
        )),
        Ok("Basic YWxpY2U6b2xk".to_owned()) // alice:old
    );
    assert_eq!(
        authorization(present_credential(
            request(),
            &Credential::bearer_for("alice", "tok-a")
        )),
        Ok("Bearer tok-a".to_owned()),
        "a token's username is not sent"
    );
}

#[test]
fn an_administrator_acting_as_another_account_is_not_presented_over_http() {
    assert_eq!(
        authorization(present_credential(request(), &administrator())),
        Err(FailureClass::Permanent)
    );
}

#[test]
fn a_token_only_api_is_sent_a_token_and_nothing_else() {
    assert_eq!(
        authorization(present_bearer(
            request(),
            &Credential::bearer_for("alice", "tok-a")
        )),
        Ok("Bearer tok-a".to_owned())
    );
    for refused in [Credential::password("alice", "old"), administrator()] {
        assert_eq!(
            authorization(present_bearer(request(), &refused)),
            Err(FailureClass::Permanent),
            "a password never goes to a server with nothing to check it against"
        );
    }
}

fn challenges(values: &[&str]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for value in values {
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_str(value).unwrap());
    }
    headers
}

#[test]
fn a_challenge_offering_the_credentials_scheme_refuses_it() {
    let token = Credential::bearer("tok-a");
    // Microsoft Graph's and Google's challenges to a token they refused, as observed live
    // (Graph's `client_id` is a placeholder).
    let graph = r#"Bearer realm="", authorization_uri="https://login.microsoftonline.com/common/oauth2/authorize", client_id="00000000-0000-0000-0000-000000000000""#;
    let google = r#"Bearer realm="https://accounts.google.com/", error="invalid_token""#;
    for challenge in [graph, google] {
        assert_eq!(
            judge_challenge(&challenges(&[challenge]), &token),
            Unauthorized::Refused
        );
    }
    assert_eq!(
        judge_challenge(
            &challenges(&[r#"Basic realm="dav""#]),
            &Credential::password("alice", "old")
        ),
        Unauthorized::Refused
    );
}

#[test]
fn a_401_without_a_challenge_refuses_the_credential() {
    // YAHOO-1: a server that sends no challenge has still judged the credential.
    assert_eq!(
        judge_challenge(&HeaderMap::new(), &Credential::bearer("tok-a")),
        Unauthorized::Refused
    );
}

#[test]
fn a_challenge_only_for_other_schemes_is_final() {
    assert_eq!(
        judge_challenge(
            &challenges(&[r#"Basic realm="dav""#]),
            &Credential::bearer("tok-a")
        ),
        Unauthorized::Final
    );
    assert_eq!(
        judge_challenge(&challenges(&["Bearer"]), &administrator()),
        Unauthorized::Final,
        "a kind HTTP cannot carry is never refused, and so never replaced"
    );
}
