//! Where a JMAP account's credential may go, beyond what the server says: an account
//! configured over `https` never sends it over plaintext, loopback included, and the source's
//! `permits` can refuse one origin the session advertises while the rest work. Also which
//! `401`s ask for nothing new: a token refused with a challenge for Basic alone, and a second
//! re-framing, which is neither sent nor latched.

use std::sync::Arc;

use engine_core::error::FailureClass;
use engine_provider::{CredentialSource, Origin};
use reqwest::Url;

use super::{
    Transport,
    credential_tests::{
        BASIC_CHALLENGE, Rotating, authorizations, config, transport as loopback_transport,
    },
    transport_tests::{
        EXPECTED_BASIC, EXPECTED_BEARER, FASTMAIL_CHALLENGE, mock_server, ok, tls, unauthorized,
    },
};
use crate::{JmapClient, JmapError, SessionUrlPolicy};

/// A transport for an account the host configured over `https`.
fn https_account(source: &Arc<Rotating>) -> Transport {
    Transport::new(
        &Url::parse("https://jmap.example.com/").unwrap(),
        Arc::clone(source) as Arc<dyn CredentialSource>,
        tls(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .unwrap()
}

fn refused_permanently<T>(result: Result<T, JmapError>) {
    let Err(err) = result else {
        panic!("refused before sending")
    };
    assert!(matches!(err, JmapError::Credential(_)), "{err:?}");
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[tokio::test]
async fn an_https_account_sends_nothing_to_plaintext_loopback_its_session_advertises() {
    // A session served over https may name `http://127.0.0.1:…` for its push stream, its
    // downloads or its uploads, kept verbatim where it is another origin than the API's.
    // Whatever listens there on this machine would read the credential in cleartext.
    let (loopback, seen) = mock_server(vec![ok("{}")]);
    let source = Arc::new(Rotating::tokens("a", &[]));
    let transport = https_account(&source);
    refused_permanently(transport.get_event_stream(&loopback).await);
    refused_permanently(transport.get_bytes(&loopback).await);
    refused_permanently(
        transport
            .post_bytes(&loopback, "text/plain", b"x".to_vec())
            .await,
    );
    assert_eq!(seen.lock().unwrap().len(), 0, "nothing reached loopback");
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn a_token_refused_with_a_challenge_for_basic_alone_is_not_replaced() {
    // Another token would be refused the same way: replacing it would refresh the token on
    // every request against a server that never takes Bearer.
    let (base, seen) = mock_server(vec![unauthorized(BASIC_CHALLENGE), ok("{}")]);
    let source = Arc::new(Rotating::tokens("a", &["b"]));
    let reply = loopback_transport(&source)
        .get(&base)
        .await
        .expect("answered");
    assert_eq!(reply.status(), 401);
    assert_eq!(authorizations(&seen), ["Bearer a"]);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn a_second_reframing_is_neither_sent_nor_latched() {
    // Basic → Bearer is re-framed and latched; the server then asks for Basic again, which the
    // funnel does not send, so the latch must stay on the framing that went out.
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        unauthorized(BASIC_CHALLENGE),
        ok("{}"),
    ]);
    let source = Arc::new(Rotating::new(
        engine_provider::Credential::password("alice@example.com", "s3cret"),
        Vec::new(),
    ));
    let transport = loopback_transport(&source);
    let reply = transport.get(&base).await.expect("answered");
    assert_eq!(reply.status(), 401);
    transport.get(&base).await.expect("answered");
    assert_eq!(
        authorizations(&seen),
        [EXPECTED_BASIC, EXPECTED_BEARER, EXPECTED_BEARER]
    );
}

#[tokio::test]
async fn permits_can_refuse_a_download_origin_the_session_advertises_while_the_api_works() {
    let session = r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"/jmap/","downloadUrl":"https://download.example.net/{accountId}/{blobId}/{name}?accept={type}"}"#;
    let (base, seen) = mock_server(vec![ok(session), ok(r#"{"methodResponses":[]}"#)]);
    let api = Url::parse(&base).unwrap();
    let mut source = Rotating::tokens("a", &[]);
    source.permitted = Some(vec![Origin::new(
        "http",
        api.host_str().unwrap(),
        api.port().unwrap(),
    )]);
    let source = Arc::new(source);
    let client = JmapClient::connect(
        config(&base, &source).with_session_urls(SessionUrlPolicy::TrustAdvertised),
    )
    .await
    .expect("the API's origin is permitted");
    client
        .execute(&crate::Request::new([crate::capability::CORE]))
        .await
        .expect("the API works");
    refused_permanently(
        client
            .download("https://download.example.net/c/b/n?accept=x")
            .await,
    );
    assert_eq!(seen.lock().unwrap().len(), 2, "the session and the call");
    assert_eq!(source.asks(), 2, "never asked for the refused origin");
}
