//! Every Graph request that carries the credential asks the account's source for it, through
//! `engine_http::AuthenticatedClient`: a token Graph refuses is replaced once and the request sent
//! once more with the replacement, and nothing else is replayed. An origin the rules refuse is
//! sent nothing, and a content URL never carries the credential. The server records every
//! request, so each test asserts what was actually sent.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_http::test_server::{authorization, scripted};
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, HandOver, Origin,
    ProviderError, Unrecorded,
};

use crate::{
    GraphClient,
    test_support::{retry, tls, user_agent},
};

/// A source that answers `first`, then each of `replacements` when asked to replace, permits
/// only `hosts` (every host when empty), and counts what it was asked.
#[derive(Debug)]
struct Tokens {
    subject: CredentialSubject,
    first: Credential,
    replacements: Mutex<VecDeque<Credential>>,
    hosts: Vec<&'static str>,
    asks: Mutex<usize>,
    replaced: Mutex<usize>,
}

impl Tokens {
    fn new(first: Credential, replacements: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("acct-1"),
            first,
            replacements: Mutex::new(
                replacements
                    .iter()
                    .map(|token| Credential::bearer(*token))
                    .collect(),
            ),
            hosts: Vec::new(),
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        })
    }

    fn rotating(first: &str, replacements: &[&str]) -> Arc<Self> {
        Self::new(Credential::bearer(first), replacements)
    }

    fn only(host: &'static str) -> Arc<Self> {
        let mut tokens = Arc::into_inner(Self::rotating("tok-a", &["tok-b"])).expect("one owner");
        tokens.hosts = vec![host];
        Arc::new(tokens)
    }

    fn asks(&self) -> usize {
        *self.asks.lock().unwrap()
    }

    fn replaced(&self) -> usize {
        *self.replaced.lock().unwrap()
    }
}

#[async_trait]
impl CredentialSource for Tokens {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, origin: &Origin) -> bool {
        self.hosts.is_empty() || self.hosts.contains(&origin.host())
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        *self.asks.lock().unwrap() += 1;
        Ok(self.first.clone())
    }

    async fn replace(
        &self,
        _: &Credential,
        _: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        *self.replaced.lock().unwrap() += 1;
        Ok(self.replacements.lock().unwrap().pop_front())
    }
}

fn client(base: &str, source: &Arc<Tokens>) -> GraphClient {
    GraphClient::with_base_and_source(
        Arc::clone(source) as Arc<dyn CredentialSource>,
        base,
        tls(),
        &retry(),
        &user_agent(),
    )
    .expect("client")
}

/// The `Authorization` each recorded request carried, in order.
fn presented(seen: &Mutex<Vec<String>>) -> Vec<Option<String>> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|request| authorization(request))
        .collect()
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The `401` Graph answered a token it refused, as observed live: its challenge
/// (`tests/fixtures/error/unauthorized_challenge.txt`) and its body (`unauthorized.json`).
fn refused() -> String {
    const CHALLENGE: &str = include_str!("../tests/fixtures/error/unauthorized_challenge.txt");
    const BODY: &str = include_str!("../tests/fixtures/error/unauthorized.json");
    format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{BODY}",
        CHALLENGE.trim_end(),
        BODY.len()
    )
}

fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_refused_token_is_replaced_once_and_the_request_sent_again_with_it() {
    let (base, seen) = scripted(vec![refused(), reply("200 OK", r#"{"value":[]}"#)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let page = client
        .get(&client.url("/mailFolders"))
        .await
        .expect("a page");

    assert!(page["value"].is_array());
    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert_eq!((source.asks(), source.replaced()), (1, 1));
}

#[tokio::test]
async fn a_submission_refused_is_sent_again_with_the_replacement() {
    let (base, seen) = scripted(vec![refused(), reply("202 Accepted", "")]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let answer = client
        .submit(
            &client.url("/sendMail"),
            "text/plain",
            b"bWltZQ==".to_vec(),
            &HandOver::new(&Unrecorded),
        )
        .await
        .expect("sent");

    assert_eq!(answer, None);
    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert!(
        seen.lock().unwrap()[1].ends_with("bWltZQ=="),
        "the replay carries the body"
    );
    assert_eq!((source.asks(), source.replaced()), (1, 1));
}

#[tokio::test]
async fn a_replacement_refused_too_is_an_authentication_failure() {
    let (base, seen) = scripted(vec![refused(), refused()]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client.get(&client.url("/mailFolders")).await.unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert_eq!(source.replaced(), 1, "one replacement, never two");
}

#[tokio::test]
async fn a_403_is_not_a_refusal_of_the_token() {
    let body = r#"{"error":{"code":"ErrorAccessDenied","message":"Access is denied."}}"#;
    let (base, seen) = scripted(vec![reply("403 Forbidden", body)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client.get(&client.url("/mailFolders")).await.unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert_eq!(presented(&seen), [Some(bearer("tok-a"))]);
    assert_eq!(source.replaced(), 0);
}

/// MICROSOFT-2: Graph answers a source a personal account does not have with a `401` and no
/// challenge. A new token is refused the same way, so none is asked for.
#[tokio::test]
async fn a_401_without_a_challenge_is_not_a_refusal_of_the_token() {
    const BODY: &str = include_str!("../tests/fixtures/error/contacts_directory_unauthorized.json");
    let (base, seen) = scripted(vec![reply("401 Unauthorized", BODY)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client
        .get(&client.global_url("/users/delta"))
        .await
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(presented(&seen), [Some(bearer("tok-a"))]);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn a_failed_exchange_asks_for_no_replacement_and_is_not_sent_again() {
    // The script's one reply closes the connection unanswered, after the request arrived.
    let (base, seen) = scripted(vec![String::new()]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client
        .post(&client.url("/messages"), "application/json", b"{}".to_vec())
        .await
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Retryable);
    assert_eq!(presented(&seen), [Some(bearer("tok-a"))]);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_is_sent_nothing() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::only("graph.example.com");
    let client = client(&base, &source);

    let err = client.get(&client.url("/mailFolders")).await.unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn plaintext_off_loopback_is_sent_nothing() {
    // An account on the real Graph root, handed an `http://` link to a loopback server.
    let (listening, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client("https://graph.example.com/v1.0", &source);

    let err = client
        .get(&format!("{listening}/v1.0/me"))
        .await
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn a_password_is_never_sent_to_graph() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::new(Credential::password("alice", "secret"), &[]);
    let client = client(&base, &source);

    let err = client.get(&client.url("/mailFolders")).await.unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
}

#[tokio::test]
async fn a_content_url_carries_no_credential_and_asks_for_none() {
    let (photos, seen) = scripted(vec![reply("200 OK", "bytes")]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client("https://graph.example.com/v1.0", &source);

    let bytes = client
        .get_bytes(&format!("{photos}/photo.jpg"))
        .await
        .expect("bytes");

    assert_eq!(bytes, b"bytes");
    assert_eq!(presented(&seen), [None]);
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn clones_of_a_client_share_one_transport() {
    // One source asked for both: the clone a second folder's provider holds is the same client.
    let (base, seen) = scripted(vec![reply("200 OK", "{}"), reply("200 OK", "{}")]);
    let source = Tokens::rotating("tok-a", &[]);
    let client = client(&base, &source);
    let other = client.clone();

    client.get(&client.url("/me")).await.expect("first");
    assert!(
        other.http_version().is_some(),
        "the clone saw the connection the first request was answered over"
    );
    other.get(&other.url("/me")).await.expect("second");

    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-a"))]
    );
    assert_eq!(source.asks(), 2, "asked per request, not per client");
}
