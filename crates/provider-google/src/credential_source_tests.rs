//! Every Google API request that carries the credential asks the account's source for it,
//! through `engine_http::AuthenticatedClient`: a token Google refuses is replaced once and the
//! request sent once more with the replacement, and nothing else is replayed. An origin the
//! rules refuse is sent nothing, a content URL never carries the credential, and the account's
//! IMAP half is bound only when it asks the same source. The server records every request, so
//! each test asserts what was actually sent.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_http::test_server::{authorization, scripted};
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, HandOver, Origin,
    ProviderError, ProviderResult, Unrecorded,
};
use provider_imap::{
    GmailFolder, GmailFolderState, GmailMessage, GmailQuery, GmailSources, GmailState,
};

use crate::{
    GmailProvider, GoogleClient, GoogleError,
    imap_mail::ImapMail,
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
    fn new(first: Credential, replacements: &[&str], hosts: Vec<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("acct-1"),
            first,
            replacements: Mutex::new(
                replacements
                    .iter()
                    .map(|token| Credential::bearer(*token))
                    .collect(),
            ),
            hosts,
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        })
    }

    fn rotating(first: &str, replacements: &[&str]) -> Arc<Self> {
        Self::new(Credential::bearer(first), replacements, Vec::new())
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

fn client(base: &str, source: &Arc<Tokens>) -> GoogleClient {
    GoogleClient::with_base_and_source(
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

/// The `401` Google answered a token it refused, as observed live: its challenge
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
    let (base, seen) = scripted(vec![refused(), reply("200 OK", r#"{"labels":[]}"#)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let labels = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .expect("labels");

    assert!(labels["labels"].is_array());
    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert_eq!((source.asks(), source.replaced()), (1, 1));
}

#[tokio::test]
async fn a_submission_refused_is_sent_again_with_the_replacement() {
    let (base, seen) = scripted(vec![refused(), reply("200 OK", r#"{"id":"m1"}"#)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let answer = client
        .submit(
            &client.url("/gmail/v1/users/me/messages/send"),
            "application/json",
            br#"{"raw":"bWltZQ"}"#.to_vec(),
            &HandOver::new(&Unrecorded),
        )
        .await
        .expect("sent");

    assert_eq!(answer.expect("an echo")["id"], "m1");
    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-b"))]
    );
    assert!(
        seen.lock().unwrap()[1].ends_with(r#"{"raw":"bWltZQ"}"#),
        "the replay carries the body"
    );
}

#[tokio::test]
async fn a_replacement_refused_too_is_an_authentication_failure() {
    let (base, seen) = scripted(vec![refused(), refused()]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(presented(&seen).len(), 2);
    assert_eq!(source.replaced(), 1, "one replacement, never two");
}

#[tokio::test]
async fn a_403_is_not_a_refusal_of_the_token() {
    const BODY: &str = include_str!("../tests/fixtures/error/forbidden.json");
    let (base, seen) = scripted(vec![reply("403 Forbidden", BODY)]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client(&base, &source);

    let err = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .unwrap_err();

    assert!(matches!(err, GoogleError::Status { status: 403, .. }));
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
        .post(
            &client.url("/gmail/v1/users/me/labels"),
            "application/json",
            b"{}".to_vec(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.failure_class(), FailureClass::Retryable);
    assert_eq!(presented(&seen), [Some(bearer("tok-a"))]);
    assert_eq!(source.replaced(), 0);
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_is_sent_nothing() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::new(Credential::bearer("tok-a"), &[], vec!["www.example.com"]);
    let client = client(&base, &source);

    let err = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .unwrap_err();

    assert_eq!(ProviderError::from(err).class(), FailureClass::Permanent);
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn plaintext_off_loopback_is_sent_nothing() {
    // An account on an `https` base, handed an `http://` URL of a loopback server.
    let (listening, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::rotating("tok-a", &["tok-b"]);
    let client = client("https://www.example.com", &source);

    let err = client
        .get(&format!("{listening}/gmail/v1/users/me/labels"))
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
async fn a_password_is_never_sent_to_google() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
    let source = Tokens::new(Credential::password("alice", "secret"), &[], Vec::new());
    let client = client(&base, &source);

    let err = client
        .get(&client.url("/gmail/v1/users/me/labels"))
        .await
        .unwrap_err();

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
    let client = client("https://www.example.com", &source);

    let bytes = client
        .get_bytes(&format!("{photos}/photo.jpg"))
        .await
        .expect("bytes");

    assert_eq!(bytes, b"bytes");
    assert_eq!(presented(&seen), [None]);
    assert_eq!(source.asks(), 0);
}

#[test]
fn a_credential_failure_keeps_the_class_its_source_gave() {
    // A broker that is down is retried; nothing was sent, so a submission it stopped is not
    // left needing confirmation.
    let failure = GoogleError::Credential(ProviderError::retryable("the broker is down"));
    assert_eq!(failure.failure_class(), FailureClass::Retryable);
    assert!(!failure.leaves_outcome_unknown());
    let neutral = ProviderError::from(failure);
    assert_eq!(neutral.class(), FailureClass::Retryable);
    assert_eq!(
        neutral.detail(),
        "the broker is down",
        "passed through as it came"
    );
}

/// An IMAP half that is never read: only who it signs in as matters here.
struct Unread;

#[async_trait]
impl ImapMail for Unread {
    async fn open(&self, _: GmailFolder) -> ProviderResult<Option<GmailFolderState>> {
        unreachable!("never read")
    }

    async fn search(&self, _: GmailFolder, _: u32, _: &GmailQuery) -> ProviderResult<Vec<u32>> {
        unreachable!("never read")
    }

    async fn messages(
        &self,
        _: GmailFolder,
        _: u32,
        _: &[u32],
    ) -> ProviderResult<Vec<GmailMessage>> {
        unreachable!("never read")
    }

    async fn changed(
        &self,
        _: GmailFolder,
        _: u32,
        _: u32,
        _: u64,
    ) -> ProviderResult<Vec<GmailState>> {
        unreachable!("never read")
    }

    async fn ids(&self, _: GmailFolder, _: u32, _: &[u32]) -> ProviderResult<Vec<(u32, u64)>> {
        unreachable!("never read")
    }

    fn sources<'a>(&'a self, _: &'a [u64]) -> GmailSources<'a> {
        unreachable!("never read")
    }
}

#[test]
fn the_imap_half_is_bound_only_when_it_asks_the_same_source() {
    let source = Tokens::rotating("tok-a", &[]);
    let provider = || GmailProvider::new(client("https://www.example.com", &source));

    let same = provider().bind_imap(source.subject(), Arc::new(Unread));
    assert!(same.is_ok(), "one source for both halves");

    let other = provider().bind_imap(&CredentialSubject::new("acct-2"), Arc::new(Unread));
    assert_eq!(
        other.map(|_| ()).unwrap_err().class(),
        FailureClass::InvalidState,
        "another account's IMAP half is refused"
    );

    // A fixed token signs in under a subject of its own, which no IMAP half shares.
    let fixed = GmailProvider::new(
        GoogleClient::with_base(
            "tok-a",
            "https://www.example.com",
            tls(),
            &retry(),
            &user_agent(),
        )
        .expect("client"),
    );
    assert!(fixed.bind_imap(source.subject(), Arc::new(Unread)).is_err());
}

#[tokio::test]
async fn clones_of_a_client_share_one_transport() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}"), reply("200 OK", "{}")]);
    let source = Tokens::rotating("tok-a", &[]);
    let client = client(&base, &source);
    let other = client.clone();

    client.get(&client.url("/a")).await.expect("first");
    assert!(
        other.http_version().is_some(),
        "the clone saw the connection the first request was answered over"
    );
    other.get(&other.url("/b")).await.expect("second");

    assert_eq!(
        presented(&seen),
        [Some(bearer("tok-a")), Some(bearer("tok-a"))]
    );
    assert_eq!(source.asks(), 2, "asked per request, not per client");
}
