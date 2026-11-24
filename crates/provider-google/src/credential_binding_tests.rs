//! What a client does with what the credential funnel leaves alone (a redirect, a `401` with no
//! challenge), a base it cannot use, and an IMAP half: [`GmailProvider::with_imap`] binds a
//! real reader, connected offline to `provider_imap::test_server::gmail`, only when it signs in
//! as the same credential subject as the client.

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_http::test_server::scripted;
use engine_provider::{Credential, CredentialSource, CredentialSubject, FixedCredential};
use provider_imap::{GmailMail, ImapAccount, ImapConfig};
use tokio::io::{AsyncRead, AsyncWrite};

use super::{Tokens, bearer, client, presented, reply};
use crate::{
    GmailProvider, GoogleClient, GoogleError,
    test_support::{retry, tls, user_agent},
};

#[tokio::test]
async fn an_authenticated_redirect_comes_back_as_it_came() {
    let (base, seen) = scripted(vec![
        "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
            .to_owned(),
        reply("200 OK", "{}"),
    ]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .unwrap_err();

    assert!(
        matches!(err, GoogleError::Status { status: 302, .. }),
        "{err}"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the redirect was not followed"
    );
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn a_401_without_a_challenge_is_a_refusal_of_the_token() {
    // YAHOO-1's rule, which Google's API keeps: a bare `401` has judged the token.
    let (base, seen) = scripted(vec![
        reply("401 Unauthorized", ""),
        reply("200 OK", r#"{"labels":[]}"#),
    ]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .expect("labels");

    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert_eq!(source.replaced(), 1);
}

#[test]
fn a_base_that_is_not_an_absolute_url_is_refused() {
    for base in ["not a url", "/gmail/v1"] {
        let fixed = GoogleClient::with_base("tok", base, tls(), &retry(), &user_agent());
        assert!(matches!(fixed, Err(GoogleError::Protocol(_))), "{base}");
        let source = Tokens::rotating("tok-a", &[]);
        let sourced = GoogleClient::with_base_and_source(
            source as Arc<dyn CredentialSource>,
            base,
            tls(),
            &retry(),
            &user_agent(),
        );
        assert!(matches!(sourced, Err(GoogleError::Protocol(_))), "{base}");
    }
}

/// A reader of the in-process Gmail server, signing in as `subject`.
async fn reader(
    subject: &str,
) -> GmailMail<impl AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> {
    let (cert, addr) = provider_imap::test_server::gmail().await;
    let tls = engine_tls::client_config(&engine_tls::TlsPolicy::pinned(vec![cert])).expect("tls");
    let source = FixedCredential::new(
        CredentialSubject::new(subject),
        Credential::password("alice", "pw"),
    );
    let config = ImapConfig::from_credential_source(addr, "127.0.0.1", Arc::new(source));
    ImapAccount::connect(&config, tls.connector())
        .await
        .expect("connect")
        .gmail()
        .expect("X-GM-EXT-1")
}

#[tokio::test]
async fn the_imap_half_is_bound_when_it_signs_in_as_the_clients_subject() {
    // The client's source names `acct-1`; so does the reader's, a separate source object. By
    // the subject contract they are one account's.
    let source = Tokens::rotating("tok-a", &[]);
    let provider = GmailProvider::new(client("https://www.example.com", &source));

    assert!(provider.with_imap(reader("acct-1").await).is_ok());
}

#[tokio::test]
async fn the_imap_half_is_refused_when_it_signs_in_as_another_subject() {
    let source = Tokens::rotating("tok-a", &[]);
    let provider = GmailProvider::new(client("https://www.example.com", &source));

    let refused = provider.with_imap(reader("acct-2").await);

    assert_eq!(
        refused.map(|_| ()).unwrap_err().class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn a_fixed_token_shares_its_subject_with_no_imap_half() {
    // `connect(token)` and `with_base` sign in under a subject of their own.
    let fixed = GoogleClient::with_base(
        "tok-a",
        "https://www.example.com",
        tls(),
        &retry(),
        &user_agent(),
    )
    .expect("client");

    let refused = GmailProvider::new(fixed).with_imap(reader("acct-1").await);

    assert_eq!(
        refused.map(|_| ()).unwrap_err().class(),
        FailureClass::InvalidState
    );
}
