use reqwest::{
    Url,
    header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE},
};

use super::{CredentialScope, offers};
use crate::transport::Credentials;

fn challenges(values: &[&str]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for value in values {
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_str(value).unwrap());
    }
    headers
}

fn basic() -> Credentials {
    Credentials::Basic {
        username: "alice".to_owned(),
        password: "pw".to_owned(),
    }
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
fn a_401_without_any_challenge_still_asks() {
    // Yahoo's calendar server answers 401 with no `WWW-Authenticate` at all.
    assert!(offers(&HeaderMap::new(), "basic"));
    assert!(offers(&HeaderMap::new(), "bearer"));
}

#[test]
fn no_origin_is_sent_the_credential_before_it_asks() {
    let configured = Url::parse("https://dav.example.net/").unwrap();
    let home = Url::parse("https://p1.dav.example.net/u/calendars/").unwrap();
    let scope = CredentialScope::new(&configured);

    let anywhere = configured.join("/anything").unwrap();
    assert!(!scope.sends_to(&anywhere), "not even the configured server");
    assert!(scope.answers(
        &configured,
        &challenges(&[r#"Basic realm="cal""#]),
        &basic()
    ));
    assert!(scope.sends_to(&anywhere));
    scope.trust(&home);
    assert!(!scope.sends_to(&home), "trusted is not the same as asked");
    assert!(scope.answers(&home, &challenges(&[r#"Basic realm="cal""#]), &basic()));
    assert!(
        scope.sends_to(&home),
        "an origin that asked keeps receiving it"
    );
}

#[test]
fn an_origin_discovery_never_reached_is_not_answered_however_it_asks() {
    let scope = CredentialScope::new(&Url::parse("https://dav.example.net/").unwrap());
    let photo = Url::parse("https://photos.example.org/ada.png").unwrap();

    assert!(!scope.answers(&photo, &challenges(&[r#"Basic realm="x""#]), &basic()));
    assert!(!scope.sends_to(&photo));
}

#[test]
fn a_challenge_for_another_scheme_is_not_answered() {
    let configured = Url::parse("https://dav.example.net/").unwrap();
    let home = Url::parse("https://p1.dav.example.net/").unwrap();
    let scope = CredentialScope::new(&configured);
    scope.trust(&home);

    let token = Credentials::Bearer("token".to_owned());
    assert!(!scope.answers(&home, &challenges(&[r#"Basic realm="cal""#]), &token));
    assert!(!scope.sends_to(&home));
}
