use engine_provider::Credential;
use reqwest::header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE};

use super::{offered, offers, scheme_of};
use crate::transport::Credentials;

fn challenges(values: &[&str]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for value in values {
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_str(value).unwrap());
    }
    headers
}

#[test]
fn a_challenge_is_found_in_either_shape_a_server_sends() {
    // One challenge per field, and several in one field, as two real servers send them.
    assert!(offers(
        &challenges(&[r#"X-Token realm="cal""#, r#"Basic realm="cal""#]),
        "basic"
    ));
    assert!(offers(
        &challenges(&[r#"X-Token realm="book", Basic realm="book""#]),
        "basic"
    ));
    assert!(!offers(&challenges(&[r#"Basic realm="cal""#]), "bearer"));
    assert!(
        !offers(
            &challenges(&[r#"Digest realm="basic", qop="auth""#]),
            "basic"
        ),
        "a parameter value is not a scheme"
    );
}

#[test]
fn a_comma_inside_a_quoted_value_separates_nothing() {
    assert!(
        !offers(
            &challenges(&[
                r#"Bearer realm="cal", error_description="expired, basic auth refused""#
            ]),
            "basic"
        ),
        "a word after a quoted comma is not a scheme"
    );
    assert!(!offers(
        &challenges(&[r#"Digest realm="a \"q, Basic\" b", qop="auth""#]),
        "basic"
    ));
}

#[test]
fn a_challenge_with_a_non_ascii_realm_is_still_read() {
    let mut headers = challenges(&["Negotiate"]);
    headers.append(
        WWW_AUTHENTICATE,
        HeaderValue::from_bytes("Basic realm=\"Kalender für alle\"".as_bytes()).unwrap(),
    );
    assert!(offers(&headers, "basic"));
}

#[test]
fn a_401_without_any_challenge_still_asks() {
    // Yahoo's calendar server answers 401 with no `WWW-Authenticate` at all.
    assert!(offers(&HeaderMap::new(), "basic"));
    assert!(offers(&HeaderMap::new(), "bearer"));
}

#[test]
fn the_offered_schemes_keep_the_servers_spelling_across_fields() {
    let headers = challenges(&[r#"Digest realm="cal", qop="auth""#, "NTLM"]);
    assert_eq!(offered(&headers), ["Digest", "NTLM"]);
}

#[test]
fn a_password_is_basic_a_token_bearer_and_an_acting_as_nothing() {
    let password: Credential = Credentials::Basic {
        username: "alice".to_owned(),
        password: "pw".to_owned(),
    }
    .into();
    let token: Credential = Credentials::Bearer("token".to_owned()).into();
    assert_eq!(scheme_of(&password), Some("basic"));
    assert_eq!(scheme_of(&token), Some("bearer"));
    assert_eq!(
        scheme_of(&Credential::acting_as("admin", "pw", "alice")),
        None,
        "HTTP has no authorization identity to carry it in"
    );
}
