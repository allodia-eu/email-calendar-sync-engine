//! Every authenticated JMAP request asks the account's credential source, through
//! `engine_http::AuthenticatedClient::send`: a refused credential is replaced once and the request
//! replayed with the replacement, re-framing and replacement never answer the same `401`, and
//! an origin the rules refuse (the source's `permits`, plaintext off loopback) is sent nothing,
//! whether it was configured, reached by a redirect, or advertised by the session.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, Origin, ProviderError,
};
use serde_json::json;

use super::{
    Transport,
    transport_tests::{
        EXPECTED_BASIC, EXPECTED_BEARER, FASTMAIL_CHALLENGE, http, mock_server, ok, tls,
        unauthorized,
    },
};
use crate::{JmapClient, JmapConfig, JmapError, SessionUrlPolicy};

/// A source that answers `first`, then each of `replacements` in turn when asked to replace,
/// and counts what it was asked.
#[derive(Debug)]
pub(super) struct Rotating {
    subject: CredentialSubject,
    first: Credential,
    replacements: Mutex<VecDeque<Credential>>,
    /// The origins it permits; every one when `None`.
    pub(super) permitted: Option<Vec<Origin>>,
    /// What its asks fail with, if they fail.
    fails: Option<FailureClass>,
    asks: Mutex<Vec<Origin>>,
    replaced: Mutex<usize>,
}

impl Rotating {
    pub(super) fn new(first: Credential, replacements: Vec<Credential>) -> Self {
        Self {
            subject: CredentialSubject::new("acct-1"),
            first,
            replacements: Mutex::new(replacements.into()),
            permitted: None,
            fails: None,
            asks: Mutex::new(Vec::new()),
            replaced: Mutex::new(0),
        }
    }

    pub(super) fn tokens(first: &str, replacements: &[&str]) -> Self {
        Self::new(
            Credential::bearer(first),
            replacements
                .iter()
                .map(|t| Credential::bearer(*t))
                .collect(),
        )
    }

    pub(super) fn asks(&self) -> usize {
        self.asks.lock().unwrap().len()
    }

    pub(super) fn replaced(&self) -> usize {
        *self.replaced.lock().unwrap()
    }
}

#[async_trait]
impl CredentialSource for Rotating {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, origin: &Origin) -> bool {
        self.permitted
            .as_ref()
            .is_none_or(|permitted| permitted.contains(origin))
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        self.asks.lock().unwrap().push(request.origin.clone());
        match self.fails {
            Some(class) => Err(ProviderError::new(class, "the broker said no")),
            None => Ok(self.first.clone()),
        }
    }

    async fn replace(
        &self,
        _refused: &Credential,
        _request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        *self.replaced.lock().unwrap() += 1;
        Ok(self.replacements.lock().unwrap().pop_front())
    }
}

pub(super) fn transport(source: &Arc<Rotating>) -> Transport {
    Transport::new(
        &reqwest::Url::parse("http://127.0.0.1/").unwrap(),
        Arc::clone(source) as Arc<dyn CredentialSource>,
        tls(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .unwrap()
}

pub(super) fn authorizations(seen: &Mutex<Vec<super::transport_tests::Seen>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|seen| seen.authorization.clone())
        .collect()
}

pub(super) const BEARER_CHALLENGE: &str = r#"Bearer realm="jmap""#;
pub(super) const BASIC_CHALLENGE: &str = r#"Basic realm="jmap""#;

#[tokio::test]
async fn a_refused_token_is_replaced_once_and_the_api_call_replayed_with_the_replacement() {
    let (base, seen) = mock_server(vec![
        unauthorized(BEARER_CHALLENGE),
        ok(r#"{"methodResponses":[]}"#),
    ]);
    let source = Arc::new(Rotating::tokens("token-a", &["token-b"]));
    let envelope = json!({"using": [], "methodCalls": []});
    transport(&source)
        .post_json(&base, &envelope, engine_http::Exchange::Ordinary)
        .await
        .expect("replayed");
    assert_eq!(authorizations(&seen), ["Bearer token-a", "Bearer token-b"]);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[1].body, seen[0].body,
        "the replay carries the same call"
    );
    assert_eq!((source.asks(), source.replaced()), (1, 1));
}

#[tokio::test]
async fn a_wrong_password_is_replaced_by_the_source_and_framed_as_before() {
    // Stalwart's shape: the challenge offers the scheme used, so the password was judged.
    let (base, seen) = mock_server(vec![unauthorized(BASIC_CHALLENGE), ok("{}")]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "wrong"),
        vec![Credential::password("alice@example.com", "s3cret")],
    ));
    transport(&source).get(&base).await.expect("replayed");
    let sent = authorizations(&seen);
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0], EXPECTED_BASIC);
    assert_eq!(sent[1], EXPECTED_BASIC);
    assert_eq!(source.replaced(), 1);
}

#[tokio::test]
async fn a_reframe_and_a_replacement_never_answer_the_same_401() {
    // The challenge asks for another framing of the same secret: re-framed, and the source
    // is not asked for anything new.
    let (base, seen) = mock_server(vec![unauthorized(FASTMAIL_CHALLENGE), ok("{}")]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "s3cret"),
        vec![Credential::password("alice@example.com", "other")],
    ));
    transport(&source).get(&base).await.expect("re-framed");
    assert_eq!(authorizations(&seen), [EXPECTED_BASIC, EXPECTED_BEARER]);
    assert_eq!(source.replaced(), 0, "a re-framing is not a refusal");
}

#[tokio::test]
async fn a_challenge_with_a_realm_that_is_not_ascii_still_asks_for_its_scheme() {
    // A Bearer-only challenge whose realm is not ASCII: read as offering Bearer, so the password
    // is re-framed, rather than read as no challenge at all and the password replaced.
    let (base, seen) = mock_server(vec![
        unauthorized("Bearer realm=\"Kalender für alle\""),
        ok("{}"),
    ]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "s3cret"),
        vec![Credential::password("alice@example.com", "other")],
    ));
    transport(&source).get(&base).await.expect("re-framed");
    assert_eq!(authorizations(&seen), [EXPECTED_BASIC, EXPECTED_BEARER]);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn a_401_that_survives_the_reframe_is_replaced_once() {
    let (base, seen) = mock_server(vec![
        unauthorized(FASTMAIL_CHALLENGE),
        unauthorized(FASTMAIL_CHALLENGE),
        ok("{}"),
    ]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "s3cret"),
        vec![Credential::bearer("fresh")],
    ));
    transport(&source).get(&base).await.expect("replaced");
    assert_eq!(
        authorizations(&seen),
        [EXPECTED_BASIC, EXPECTED_BEARER, "Bearer fresh"]
    );
    assert_eq!(source.replaced(), 1);
}

#[tokio::test]
async fn a_403_asks_for_no_replacement() {
    let (base, seen) = mock_server(vec![http("403 Forbidden", &[], "{}"), ok("{}")]);
    let source = Arc::new(Rotating::tokens("token-a", &["token-b"]));
    let reply = transport(&source).get(&base).await.expect("answered");
    assert_eq!(reply.status(), 403);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn blob_downloads_uploads_and_the_push_stream_ask_the_source_too() {
    let (base, seen) = mock_server(vec![
        unauthorized(BEARER_CHALLENGE),
        ok("raw"),
        unauthorized(BEARER_CHALLENGE),
        ok(r#"{"blobId":"G1"}"#),
        unauthorized(BEARER_CHALLENGE),
        http("200 OK", &[("Content-Type", "text/event-stream")], ""),
    ]);
    let source = Arc::new(Rotating::tokens("a", &["b", "c", "d"]));
    let transport = transport(&source);
    assert_eq!(transport.get_bytes(&base).await.expect("download"), b"raw");
    transport
        .post_bytes(&base, "text/plain", b"attachment".to_vec())
        .await
        .expect("upload");
    let stream = transport.get_event_stream(&base).await.expect("stream");
    assert_eq!(stream.status(), 200);
    assert_eq!(
        authorizations(&seen),
        [
            "Bearer a", "Bearer b", "Bearer a", "Bearer c", "Bearer a", "Bearer d"
        ]
    );
    assert_eq!((source.asks(), source.replaced()), (3, 3));
}

#[tokio::test]
async fn an_administrator_acting_as_another_account_is_sent_nowhere() {
    let (base, seen) = mock_server(vec![ok("{}")]);
    let source = Arc::new(Rotating::new(
        Credential::acting_as("admin@example.com", "s3cret", "alice@example.com"),
        Vec::new(),
    ));
    let Err(err) = transport(&source).get(&base).await else {
        panic!("refused")
    };
    assert_eq!(err.failure_class(), FailureClass::Permanent, "{err}");
    assert_eq!(seen.lock().unwrap().len(), 0);
}

pub(super) fn config(base: &str, source: &Arc<Rotating>) -> JmapConfig {
    JmapConfig::from_credential_source(
        base,
        Arc::clone(source) as Arc<dyn CredentialSource>,
        engine_http::test_server::user_agent(),
    )
    .with_tls(tls().clone())
}

/// What `connect` failed with, as a host sees it.
pub(super) async fn connect_error(config: JmapConfig) -> ProviderError {
    match JmapClient::connect(config).await {
        Ok(_) => panic!("connect should fail"),
        Err(err) => err.into(),
    }
}

#[tokio::test]
async fn a_configured_plaintext_origin_off_loopback_is_sent_nothing() {
    let source = Arc::new(Rotating::tokens("a", &[]));
    let err = connect_error(config("http://jmap.example.com", &source)).await;
    assert_eq!(err.class(), FailureClass::Permanent, "{err}");
    assert_eq!(source.asks(), 0, "the source is never asked");
}

#[tokio::test]
async fn a_redirect_to_plaintext_off_loopback_is_not_followed_with_the_credential() {
    let (base, seen) = mock_server(vec![http(
        "307 Temporary Redirect",
        &[("Location", "http://jmap.example.com/jmap/session")],
        "",
    )]);
    let source = Arc::new(Rotating::tokens("a", &[]));
    let err = connect_error(config(&base, &source)).await;
    assert_eq!(err.class(), FailureClass::Permanent, "{err}");
    assert_eq!(seen.lock().unwrap().len(), 1, "only the loopback hop");
    assert_eq!(source.asks(), 1);
}

/// A session advertising every URL on a plaintext origin that is not loopback.
pub(super) const PLAINTEXT_SESSION: &str = r#"{"capabilities":{"urn:ietf:params:jmap:core":{},"urn:ietf:params:jmap:mail":{}},"primaryAccounts":{"urn:ietf:params:jmap:mail":"c"},"apiUrl":"http://jmap.example.com/jmap/","downloadUrl":"http://jmap.example.com/download/{accountId}/{blobId}/{name}?accept={type}","uploadUrl":"http://jmap.example.com/upload/{accountId}/","eventSourceUrl":"http://jmap.example.com/events/?types={types}&closeafter={closeafter}&ping={ping}"}"#;

#[tokio::test]
async fn origins_the_session_advertises_in_plaintext_off_loopback_are_sent_nothing() {
    let (base, seen) = mock_server(vec![ok(PLAINTEXT_SESSION)]);
    let source = Arc::new(Rotating::tokens("a", &[]));
    let client = JmapClient::connect(
        config(&base, &source).with_session_urls(SessionUrlPolicy::TrustAdvertised),
    )
    .await
    .expect("the session itself comes from loopback");
    let request = crate::Request::new([crate::capability::CORE]);
    let refused = [
        client.execute(&request).await.map(|_| ()),
        client
            .download("http://jmap.example.com/download/c/b/n?accept=x")
            .await
            .map(|_| ()),
        client
            .upload("http://jmap.example.com/upload/c/", "text/plain", b"x")
            .await
            .map(|_| ()),
        client
            .open_event_source(&[], core::time::Duration::from_secs(1))
            .await
            .map(|_| ()),
    ];
    for result in refused {
        let err = result.expect_err("refused before sending");
        assert!(matches!(err, JmapError::Credential(_)), "{err:?}");
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }
    assert_eq!(seen.lock().unwrap().len(), 1, "only the session request");
    assert_eq!(source.asks(), 1, "the source is asked for loopback only");
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_is_sent_nothing() {
    let (base, seen) = mock_server(vec![ok(PLAINTEXT_SESSION)]);
    let mut source = Rotating::tokens("a", &[]);
    source.permitted = Some(vec![Origin::new("https", "jmap.example.com", 443)]);
    let source = Arc::new(source);
    let err = connect_error(config(&base, &source)).await;
    assert_eq!(err.class(), FailureClass::Permanent, "{err}");
    assert_eq!(seen.lock().unwrap().len(), 0);
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn the_sources_failure_reaches_the_host_with_its_class() {
    for class in [FailureClass::Authentication, FailureClass::Retryable] {
        let (base, seen) = mock_server(vec![ok(PLAINTEXT_SESSION)]);
        let mut source = Rotating::tokens("a", &[]);
        source.fails = Some(class);
        let source = Arc::new(source);
        let err = connect_error(config(&base, &source)).await;
        assert_eq!(err.class(), class, "{err}");
        assert_eq!(seen.lock().unwrap().len(), 0, "nothing was sent");
    }
}

#[tokio::test]
async fn a_fixed_credential_built_by_new_is_presented_and_never_replaced() {
    let (base, seen) = mock_server(vec![unauthorized(BASIC_CHALLENGE)]);
    let config = JmapConfig::new(
        &base,
        crate::Credentials::basic("alice@example.com", "s3cret"),
        engine_http::test_server::user_agent(),
    )
    .with_tls(tls().clone());
    let err = connect_error(config).await;
    assert_eq!(err.class(), FailureClass::Authentication, "{err}");
    assert_eq!(authorizations(&seen), [EXPECTED_BASIC]);
}

/// Stalwart's `401` (v1.0.2) to a wrong password and to a revoked token alike, captured from the
/// harness: two challenges, each in a header of its own, Bearer first.
fn stalwart_401() -> String {
    http(
        "401 Unauthorized",
        &[
            (
                "WWW-Authenticate",
                r#"Bearer realm="Stalwart Server", resource_metadata="https://mail.test.local/.well-known/oauth-protected-resource/jmap/session""#,
            ),
            ("WWW-Authenticate", r#"Basic realm="Stalwart Server""#),
        ],
        "",
    )
}

#[tokio::test]
async fn stalwarts_401_to_a_wrong_password_is_a_refusal_and_not_a_reframe() {
    // Its first challenge names Bearer; the second names Basic, the scheme the password went
    // out under, so the password was judged. Re-framing it as a token would spend a second
    // failed sign-in on the same wrong password.
    let (base, seen) = mock_server(vec![stalwart_401(), ok("{}")]);
    let source = Arc::new(Rotating::new(
        Credential::password("alice@example.com", "wrong"),
        vec![Credential::password("alice@example.com", "s3cret")],
    ));
    transport(&source).get(&base).await.expect("replaced");
    let sent = authorizations(&seen);
    assert!(sent[0].starts_with("Basic "), "{sent:?}");
    assert_eq!(sent[1], EXPECTED_BASIC);
    assert_eq!(sent.len(), 2);
    assert_eq!(source.replaced(), 1);
}

#[tokio::test]
async fn stalwarts_401_to_a_revoked_token_is_a_refusal() {
    let (base, seen) = mock_server(vec![stalwart_401(), ok("{}")]);
    let source = Arc::new(Rotating::tokens("revoked", &["fresh"]));
    transport(&source).get(&base).await.expect("replaced");
    assert_eq!(authorizations(&seen), ["Bearer revoked", "Bearer fresh"]);
    assert_eq!(source.replaced(), 1);
}
