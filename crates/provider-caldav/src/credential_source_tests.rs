//! Every request that carries the credential asks the account's source for it, through
//! `engine_http::AuthenticatedClient::send`: a refused credential is replaced once and the request
//! sent once more with the replacement, a source's own failure keeps its class, and an origin the
//! rules refuse (the source's `permits`, plaintext off loopback or after TLS) is sent nothing.
//! The fakes assert each request, so a test answers only what was actually sent.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, Origin, ProviderError,
};

use crate::{
    CalDavConfig, CalDavError, CalDavProvider,
    test_support::{mock_server_capturing, multistatus},
    transport::{DavClient, DavExecutor, DavMethod, Precondition, WriteRequest},
};

/// A source that answers `first`, then each of `replacements` when asked to replace, permits
/// only the hosts in `hosts` (every host when empty), and counts what it was asked.
#[derive(Debug)]
struct Rotating {
    subject: CredentialSubject,
    first: Credential,
    replacements: Mutex<VecDeque<Credential>>,
    hosts: Vec<&'static str>,
    fails: Option<FailureClass>,
    asks: Mutex<usize>,
    replaced: Mutex<usize>,
}

impl Rotating {
    fn new(first: Credential, replacements: Vec<Credential>) -> Self {
        Self {
            subject: CredentialSubject::new("acct-1"),
            first,
            replacements: Mutex::new(replacements.into()),
            hosts: Vec::new(),
            fails: None,
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        }
    }

    fn passwords(first: &str, replacements: &[&str]) -> Self {
        Self::new(
            Credential::password("alice", first),
            replacements
                .iter()
                .map(|password| Credential::password("alice", *password))
                .collect(),
        )
    }

    fn asks(&self) -> usize {
        *self.asks.lock().unwrap()
    }

    fn replaced(&self) -> usize {
        *self.replaced.lock().unwrap()
    }
}

#[async_trait]
impl CredentialSource for Rotating {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, origin: &Origin) -> bool {
        self.hosts.is_empty() || self.hosts.contains(&origin.host())
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        *self.asks.lock().unwrap() += 1;
        match self.fails {
            Some(class) => Err(ProviderError::new(class, "the broker is down")),
            None => Ok(self.first.clone()),
        }
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

fn client(base: &str, source: &Arc<Rotating>) -> DavClient {
    DavClient::from_source(
        base,
        Arc::clone(source) as Arc<dyn CredentialSource>,
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

fn status(line: &str, headers: &str) -> String {
    format!("HTTP/1.1 {line}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n")
}

fn challenge(scheme: &str) -> String {
    status(
        "401 Unauthorized",
        &format!("WWW-Authenticate: {scheme} realm=\"dav\"\r\n"),
    )
}

const EMPTY: &str = r#"<d:multistatus xmlns:d="DAV:"/>"#;

/// The `Authorization` each captured request carried, in order (`None` for an anonymous one).
fn presented(seen: &Mutex<Vec<String>>) -> Vec<Option<String>> {
    seen.lock()
        .unwrap()
        .iter()
        .map(|request| {
            request.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            })
        })
        .collect()
}

/// The Basic header for `alice` with `password`: `old` or `new`.
fn basic(password: &str) -> String {
    let encoded = match password {
        "old" => "YWxpY2U6b2xk", // alice:old
        "new" => "YWxpY2U6bmV3", // alice:new
        other => panic!("no header written down for {other}"),
    };
    format!("Basic {encoded}")
}

async fn propfind(client: &DavClient) -> Result<u16, CalDavError> {
    client
        .send(DavMethod::Propfind, "/cal/", "0", String::new())
        .await
        .map(|response| response.status)
}

#[tokio::test]
async fn each_request_asks_the_source_and_a_refused_credential_is_replaced_once() {
    let (base, seen) = mock_server_capturing(vec![
        challenge("Basic"),
        multistatus(EMPTY),
        // The origin now answered, the next request carries the credential unasked, and the
        // server has since stopped taking it.
        challenge("Basic"),
        multistatus(EMPTY),
    ]);
    let source = Arc::new(Rotating::passwords("old", &["new"]));
    let client = client(&base, &source);

    assert_eq!(propfind(&client).await.expect("answered"), 207);
    assert_eq!(propfind(&client).await.expect("replaced"), 207);

    assert_eq!(
        presented(&seen),
        [
            None,
            Some(basic("old")),
            Some(basic("old")),
            Some(basic("new"))
        ],
        "anonymous, answered, refused, replaced"
    );
    assert_eq!(source.asks(), 2, "one ask per request that carried it");
    assert_eq!(source.replaced(), 1);
}

#[tokio::test]
async fn a_refused_answer_to_the_first_challenge_is_replaced_too() {
    let (base, seen) = mock_server_capturing(vec![
        challenge("Basic"),
        challenge("Basic"),
        multistatus(EMPTY),
    ]);
    let source = Arc::new(Rotating::passwords("old", &["new"]));
    let client = client(&base, &source);

    assert_eq!(propfind(&client).await.expect("replaced"), 207);
    assert_eq!(
        presented(&seen),
        [None, Some(basic("old")), Some(basic("new"))]
    );
    assert_eq!((source.asks(), source.replaced()), (1, 1));
}

#[tokio::test]
async fn a_second_refusal_comes_back_as_the_authentication_failure_it_is() {
    let (base, seen) = mock_server_capturing(vec![
        challenge("Basic"),
        challenge("Basic"),
        challenge("Basic"),
    ]);
    let source = Arc::new(Rotating::passwords("old", &["new", "newer"]));
    let client = client(&base, &source);

    assert_eq!(propfind(&client).await.expect("an answer"), 401);
    assert_eq!(presented(&seen).len(), 3, "no third presentation");
    assert_eq!(source.replaced(), 1, "one replacement per request");
}

#[tokio::test]
async fn a_403_or_a_challenge_for_another_scheme_asks_for_no_replacement() {
    let (base, seen) = mock_server_capturing(vec![challenge("Basic"), status("403 Forbidden", "")]);
    let source = Arc::new(Rotating::passwords("old", &["new"]));
    assert_eq!(propfind(&client(&base, &source)).await.expect("sent"), 403);
    assert_eq!(presented(&seen), [None, Some(basic("old"))]);

    let (base, _) = mock_server_capturing(vec![challenge("Basic"), challenge("Digest")]);
    assert_eq!(propfind(&client(&base, &source)).await.expect("sent"), 401);
    assert_eq!(
        source.replaced(),
        0,
        "neither is a refusal a replacement answers"
    );
}

#[tokio::test]
async fn a_source_that_fails_keeps_its_class_and_nothing_carries_a_credential() {
    let (base, seen) = mock_server_capturing(vec![challenge("Basic")]);
    let source = Arc::new(Rotating {
        fails: Some(FailureClass::Retryable),
        ..Rotating::passwords("old", &[])
    });

    let err = propfind(&client(&base, &source))
        .await
        .expect_err("the source failed");
    assert_eq!(err.failure_class(), FailureClass::Retryable);
    assert_eq!(ProviderError::from(err).class(), FailureClass::Retryable);
    assert_eq!(presented(&seen), [None]);
}

#[tokio::test]
async fn an_administrator_acting_as_another_account_is_refused_before_it_is_sent() {
    let (base, seen) = mock_server_capturing(vec![challenge("Basic")]);
    let source = Arc::new(Rotating::new(
        Credential::acting_as("admin", "pw", "alice"),
        Vec::new(),
    ));

    let err = propfind(&client(&base, &source))
        .await
        .expect_err("HTTP has no authorization identity");
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert_eq!(presented(&seen), [None]);
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_is_not_trusted() {
    let (base, _) = mock_server_capturing(vec![]);
    let source = Arc::new(Rotating {
        hosts: vec!["127.0.0.1"],
        ..Rotating::passwords("pw", &[])
    });
    let client = client(&base, &source);

    assert!(client.trust_origin("http://127.0.0.1:1/principals/"));
    assert!(!client.trust_origin("http://localhost:1/principals/"));
    assert_eq!(
        source.asks(),
        0,
        "deciding asks the source for no credential"
    );
}

#[tokio::test]
async fn a_connection_on_tls_never_trusts_plaintext_loopback() {
    let source = Arc::new(Rotating::passwords("pw", &[]));
    let client = client("https://dav.example.com/", &source);

    assert!(client.trust_origin("https://p1.dav.example.com/home/"));
    assert!(!client.trust_origin("http://127.0.0.1:18082/home/"));
    assert!(!client.trust_origin("http://dav.example.com/home/"));
}

#[tokio::test]
async fn a_configured_server_the_source_does_not_permit_is_never_dialled() {
    let (base, seen) = mock_server_capturing(vec![multistatus(EMPTY); 4]);
    let base = base.replace("127.0.0.1", "localhost");
    let source = Arc::new(Rotating {
        hosts: vec!["127.0.0.1"],
        ..Rotating::passwords("pw", &[])
    });
    let config =
        CalDavConfig::from_credential_source(base, source, engine_http::test_server::user_agent());

    let err = CalDavProvider::connect(config).await.expect_err("refused");
    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_plaintext_server_off_loopback_fails_the_connect_before_anything_is_sent() {
    // TEST-NET-1: never routed, and never reached, because nothing is sent.
    let config = CalDavConfig::from_credential_source(
        "http://192.0.2.1:9/",
        Arc::new(Rotating::passwords("pw", &[])),
        engine_http::test_server::user_agent(),
    );

    let err = CalDavProvider::connect(config).await.expect_err("refused");
    assert_eq!(err.failure_class(), FailureClass::Permanent);
}

#[tokio::test]
async fn a_method_no_credential_answers_is_said_without_asking_the_source() {
    let (base, seen) = mock_server_capturing(vec![status(
        "401 Unauthorized",
        "WWW-Authenticate: Digest realm=\"dav\", qop=\"auth\"\r\nWWW-Authenticate: Negotiate\r\n",
    )]);
    // A source that is down must not turn the verdict into a retry.
    let source = Arc::new(Rotating {
        fails: Some(FailureClass::Retryable),
        ..Rotating::passwords("old", &[])
    });

    let err = propfind(&client(&base, &source))
        .await
        .expect_err("no credential answers it");
    assert!(
        matches!(&err, CalDavError::UnsupportedAuthentication(offered) if offered == "Digest, Negotiate"),
        "{err:?}"
    );
    assert_eq!(source.asks(), 0, "no token is fetched for it");
    assert_eq!(presented(&seen), [None]);
}

#[tokio::test]
async fn a_configured_server_the_source_does_not_permit_is_not_answered_however_it_asks() {
    let (base, seen) = mock_server_capturing(vec![challenge("Basic")]);
    let source = Arc::new(Rotating {
        hosts: vec!["localhost"],
        ..Rotating::passwords("old", &[])
    });

    assert_eq!(propfind(&client(&base, &source)).await.expect("sent"), 401);
    assert_eq!(presented(&seen), [None]);
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn a_source_that_failed_leaves_the_origin_unanswered() {
    let (base, seen) = mock_server_capturing(vec![challenge("Basic"), challenge("Basic")]);
    let source = Arc::new(Rotating {
        fails: Some(FailureClass::Retryable),
        ..Rotating::passwords("old", &[])
    });
    let client = client(&base, &source);

    propfind(&client).await.expect_err("the source failed");
    propfind(&client).await.expect_err("and again");
    assert_eq!(
        presented(&seen),
        [None, None],
        "the next request asks again"
    );
}

#[tokio::test]
async fn a_guarded_write_refused_with_a_401_is_sent_again_whole_with_the_replacement() {
    let (base, seen) = mock_server_capturing(vec![
        challenge("Basic"),
        multistatus(EMPTY),
        challenge("Basic"),
        status("204 No Content", "ETag: \"2\"\r\n"),
    ]);
    let source = Arc::new(Rotating::passwords("old", &["new"]));
    let client = client(&base, &source);
    propfind(&client).await.expect("the origin answers");

    let reply = client
        .send_write(WriteRequest {
            method: DavMethod::Put,
            href: "/cal/event.ics".to_owned(),
            content_type: Some("text/calendar"),
            precondition: Precondition::IfMatch("\"1\"".to_owned()),
            body: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned(),
        })
        .await
        .expect("replaced and applied");

    assert_eq!(reply.status, 204);
    assert_eq!(
        presented(&seen)[2..],
        [Some(basic("old")), Some(basic("new"))]
    );
    let seen = seen.lock().unwrap();
    for put in &seen[2..] {
        assert!(put.starts_with("PUT /cal/event.ics"), "{put}");
        assert!(
            put.to_lowercase().contains("if-match: \"1\""),
            "the guard rides along: {put}"
        );
        assert!(
            put.ends_with("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
            "and the body: {put}"
        );
    }
}
