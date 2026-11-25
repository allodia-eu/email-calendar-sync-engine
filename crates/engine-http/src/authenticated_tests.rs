//! [`AuthenticatedClient::send`] against a server that records what each request carried: one ask,
//! one replacement and one replay on a refusal, and nothing sent where a rule says no.

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource,
    CredentialSubject, ProviderError,
};
use reqwest::{RequestBuilder, Url};

use super::{AuthenticatedClient, Unauthorized, is_loopback};
use crate::{Exchange, RetryConfig, SendError, Sent};

/// What a test request fails with.
#[derive(Debug)]
pub(super) enum Failure {
    Send(SendError),
    Source(ProviderError),
    Present,
}

impl From<SendError> for Failure {
    fn from(err: SendError) -> Self {
        Self::Send(err)
    }
}

impl From<CredentialFailure> for Failure {
    fn from(failure: CredentialFailure) -> Self {
        Self::Source(failure.into_error())
    }
}

/// A token source that answers from a script and counts what it was asked.
#[derive(Debug)]
pub(super) struct Tokens {
    pub(super) subject: CredentialSubject,
    pub(super) permits: bool,
    pub(super) never: bool,
    pub(super) first: &'static str,
    /// `replace` fails as retryable, as a token endpoint that is down does.
    pub(super) replace_fails: bool,
    pub(super) replacements: Mutex<VecDeque<&'static str>>,
    pub(super) asks: Mutex<Vec<CredentialProtocol>>,
    pub(super) replaced: Mutex<Vec<String>>,
    /// Every origin `permits` was asked about.
    pub(super) checked: Mutex<Vec<engine_provider::Origin>>,
}

impl Tokens {
    pub(super) fn new(first: &'static str, replacements: &[&'static str]) -> Self {
        Self {
            subject: CredentialSubject::new("acct-1"),
            permits: true,
            never: false,
            first,
            replace_fails: false,
            replacements: Mutex::new(replacements.iter().copied().collect()),
            asks: Mutex::new(Vec::new()),
            replaced: Mutex::new(Vec::new()),
            checked: Mutex::new(Vec::new()),
        }
    }

    /// This source, failing when asked to replace, as a token endpoint that is down does.
    pub(super) fn failing_replace(mut self) -> Self {
        self.replace_fails = true;
        self
    }

    fn asks(&self) -> usize {
        self.asks.lock().unwrap().len()
    }

    fn replaced(&self) -> Vec<String> {
        self.replaced.lock().unwrap().clone()
    }
}

#[async_trait]
impl CredentialSource for Tokens {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, origin: &engine_provider::Origin) -> bool {
        self.checked.lock().unwrap().push(origin.clone());
        self.permits
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        self.asks.lock().unwrap().push(request.protocol);
        if self.never {
            core::future::pending::<()>().await;
        }
        Ok(Credential::bearer(self.first))
    }

    async fn replace(
        &self,
        refused: &Credential,
        _request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        self.replaced.lock().unwrap().push(token(refused));
        if self.replace_fails {
            return Err(ProviderError::retryable("the token endpoint is down"));
        }
        Ok(self
            .replacements
            .lock()
            .unwrap()
            .pop_front()
            .map(Credential::bearer))
    }
}

pub(super) fn token(credential: &Credential) -> String {
    match credential {
        Credential::Bearer { token, .. } => token.expose().to_owned(),
        _ => String::from("?"),
    }
}

/// Serves `replies` in order, one per connection, and records each request's `Authorization`.
/// A reply of `""` closes the connection unanswered.
pub(super) fn server(replies: Vec<&'static str>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut authorization = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("authorization")
                {
                    value.trim().clone_into(&mut authorization);
                }
            }
            recorder.lock().unwrap().push(authorization);
            let reply = replies[index.min(replies.len() - 1)];
            if !reply.is_empty() {
                let _ = stream.write_all(
                    format!("HTTP/1.1 {reply}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                );
            }
        }
    });
    (format!("http://{addr}/"), seen)
}

/// Nothing recorded.
const NONE: [String; 0] = [];

pub(super) const OK: &str = "200 OK";
/// A refusal of the credential, by this suite's judge.
pub(super) const REFUSED: &str = "401 Unauthorized\r\nWWW-Authenticate: Refused";
/// A refusal of the framing.
const REFRAME: &str = "401 Unauthorized\r\nWWW-Authenticate: Reframe";
/// A refusal nothing can answer.
const FINAL: &str = "401 Unauthorized\r\nWWW-Authenticate: Final";

/// Reads this suite's challenges, which name the verdict outright.
pub(super) fn judge(sent: &Sent, _credential: &Credential) -> Unauthorized {
    match sent
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
    {
        Some("Reframe") => Unauthorized::Reframe,
        Some("Final") => Unauthorized::Final,
        _ => Unauthorized::Refused,
    }
}

pub(super) fn client() -> reqwest::Client {
    crate::client(
        &engine_tls::TlsClientConfig::bundled(),
        &crate::test_server::user_agent(),
    )
    .build()
    .expect("client")
}

/// The authenticated client of an account configured at `configured`, asking `source`.
pub(super) fn account(source: &Arc<Tokens>, configured: &str) -> AuthenticatedClient {
    AuthenticatedClient::new(
        &engine_tls::TlsClientConfig::bundled(),
        &crate::test_server::user_agent(),
        Url::parse(configured).expect("a URL"),
        Arc::clone(source) as Arc<dyn CredentialSource>,
        RetryConfig::default(),
    )
    .expect("a client")
}

/// A GET to `url`, from an account configured at `url` itself.
pub(super) async fn get(source: &Arc<Tokens>, url: &str) -> Result<u16, Failure> {
    send(source, url, client().get(url)).await
}

/// Sends `request`, built by another client, from an account configured at `configured`.
pub(super) async fn send(
    source: &Arc<Tokens>,
    configured: &str,
    request: RequestBuilder,
) -> Result<u16, Failure> {
    let sent = account(source, configured)
        .send(
            request,
            Exchange::Ordinary,
            |builder, credential| Ok::<_, Failure>(builder.bearer_auth(token(credential))),
            judge,
        )
        .await?;
    Ok(sent.status().as_u16())
}

#[tokio::test]
async fn a_refused_token_is_replaced_once_and_the_request_replayed_with_the_replacement() {
    let (url, seen) = server(vec![REFUSED, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 200);
    assert_eq!(*seen.lock().unwrap(), ["Bearer A", "Bearer B"]);
    assert_eq!(source.asks(), 1, "one ask");
    assert_eq!(
        source.replaced(),
        ["A"],
        "one replacement, for the refused token"
    );
    assert_eq!(*source.asks.lock().unwrap(), [CredentialProtocol::Http]);
}

#[tokio::test]
async fn a_replacement_refused_again_comes_back_as_the_401_it_is() {
    let (url, seen) = server(vec![REFUSED, REFUSED, OK]);
    let source = Arc::new(Tokens::new("A", &["B", "C"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 401);
    assert_eq!(*seen.lock().unwrap(), ["Bearer A", "Bearer B"]);
    assert_eq!(source.replaced(), ["A"], "never a second replacement");
}

#[tokio::test]
async fn without_a_replacement_the_401_comes_back_after_one_request() {
    let (url, seen) = server(vec![REFUSED, OK]);
    let source = Arc::new(Tokens::new("A", &[]));
    assert_eq!(get(&source, &url).await.expect("sent"), 401);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.replaced(), ["A"]);
}

#[tokio::test]
async fn a_403_is_never_a_refusal() {
    let (url, seen) = server(vec!["403 Forbidden", OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 403);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.replaced(), NONE);
}

#[tokio::test]
async fn a_failed_exchange_asks_for_no_replacement_and_is_not_replayed() {
    let (url, seen) = server(vec!["", OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    let failure = get(&source, &url).await.expect_err("the server hung up");
    assert!(
        matches!(&failure, Failure::Send(err) if !err.is_timeout()),
        "{failure:?}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.replaced(), NONE);
}

#[tokio::test]
async fn a_body_that_cannot_be_cloned_is_not_replayed_and_no_replacement_is_asked() {
    let (url, seen) = server(vec![REFUSED, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    let streamed = reqwest::Body::wrap(Streamed(Some(bytes::Bytes::from_static(b"not cloneable"))));
    let status = send(&source, &url, client().post(&url).body(streamed))
        .await
        .expect("sent");
    assert_eq!(status, 401, "the refusal comes back as it came");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        source.replaced(),
        NONE,
        "a replacement nothing could use is not asked for"
    );
}

#[tokio::test]
async fn a_reframe_presents_the_same_credential_again_and_asks_for_no_replacement() {
    let (url, seen) = server(vec![REFRAME, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 200);
    assert_eq!(*seen.lock().unwrap(), ["Bearer A", "Bearer A"]);
    assert_eq!(source.replaced(), NONE, "a reframe is not a refusal");
}

#[tokio::test]
async fn a_refusal_that_survives_the_reframe_is_replaced_once() {
    let (url, seen) = server(vec![REFRAME, REFUSED, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 200);
    assert_eq!(*seen.lock().unwrap(), ["Bearer A", "Bearer A", "Bearer B"]);
    assert_eq!(source.replaced(), ["A"]);
}

#[tokio::test]
async fn a_credential_is_reframed_at_most_once() {
    let (url, seen) = server(vec![REFRAME, REFRAME, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 401);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(source.replaced(), NONE);
}

#[tokio::test]
async fn a_final_401_comes_back_without_a_replacement() {
    let (url, seen) = server(vec![FINAL, OK]);
    let source = Arc::new(Tokens::new("A", &["B"]));
    assert_eq!(get(&source, &url).await.expect("sent"), 401);
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(source.replaced(), NONE);
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_receives_nothing() {
    let (url, seen) = server(vec![OK]);
    let source = Arc::new(Tokens {
        permits: false,
        ..Tokens::new("A", &[])
    });
    let failure = get(&source, &url).await.expect_err("refused");
    assert!(
        matches!(&failure, Failure::Source(err) if err.class() == FailureClass::Permanent),
        "{failure:?}"
    );
    assert_eq!(*seen.lock().unwrap(), NONE, "nothing reached the origin");
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn plaintext_to_a_remote_origin_carries_no_credential_and_fails_permanent() {
    let source = Arc::new(Tokens::new("A", &[]));
    for url in [
        "http://jmap.example.com/session",
        "http://192.0.2.1:8080/",
        "http://[2001:db8::1]/",
    ] {
        let failure = send(&source, "http://127.0.0.1:9/", client().get(url))
            .await
            .expect_err("refused before sending");
        assert!(
            matches!(&failure, Failure::Source(err) if err.class() == FailureClass::Permanent),
            "{url}: {failure:?}"
        );
    }
    assert_eq!(source.asks(), 0, "the source is never asked");
}

#[tokio::test]
async fn plaintext_to_loopback_still_carries_the_credential() {
    let (url, seen) = server(vec![OK]);
    let source = Arc::new(Tokens::new("A", &[]));
    assert_eq!(get(&source, &url).await.expect("sent"), 200);
    assert_eq!(*seen.lock().unwrap(), ["Bearer A"]);
}

#[test]
fn loopback_is_the_name_localhost_and_the_loopback_addresses() {
    for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.8.9.10", "[::1]"] {
        assert!(is_loopback(host), "{host}");
    }
    for host in [
        "localhost.example.com",
        "example.localhost.test",
        "10.0.0.1",
        "[::2]",
        "[2001:db8::1]",
        "128.0.0.1",
    ] {
        assert!(!is_loopback(host), "{host}");
    }
}

#[tokio::test]
async fn an_error_presenting_the_credential_sends_nothing() {
    let (url, seen) = server(vec![OK]);
    let source = Arc::new(Tokens::new("A", &[]));
    let failed = account(&source, &url)
        .send(
            client().get(&url),
            Exchange::Ordinary,
            |_, _| Err(Failure::Present),
            judge,
        )
        .await;
    assert!(matches!(failed, Err(Failure::Present)), "{failed:?}");
    assert_eq!(*seen.lock().unwrap(), NONE);
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_fails_retryable_at_the_requests_deadline() {
    let source = Arc::new(Tokens {
        never: true,
        ..Tokens::new("A", &[])
    });
    let started = tokio::time::Instant::now();
    let failure = get(&source, "http://127.0.0.1:9/")
        .await
        .expect_err("timed out");
    assert!(
        matches!(&failure, Failure::Source(err) if err.class() == FailureClass::Retryable),
        "{failure:?}"
    );
    assert!(matches!(&failure, Failure::Source(err) if err.is_credential_failure()));
    assert_eq!(
        started.elapsed(),
        engine_provider::Deadlines::STANDARD.reply(),
        "the request's deadline, no more"
    );
    assert!(Duration::from_secs(1) < started.elapsed());
}

/// A body reqwest cannot clone: one handed over as a stream rather than as bytes.
struct Streamed(Option<bytes::Bytes>);

impl http_body::Body for Streamed {
    type Data = bytes::Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: core::pin::Pin<&mut Self>,
        _cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Result<http_body::Frame<bytes::Bytes>, std::io::Error>>> {
        core::task::Poll::Ready(
            self.get_mut()
                .0
                .take()
                .map(|data| Ok(http_body::Frame::data(data))),
        )
    }
}

#[tokio::test]
async fn an_account_configured_over_https_sends_nothing_over_plaintext_loopback() {
    // A session or a redirect may name `http://127.0.0.1:…`; the credential would go in
    // cleartext to whatever listens on this machine's loopback.
    let (url, seen) = server(vec![OK]);
    let source = Arc::new(Tokens::new("A", &[]));
    let failure = send(&source, "https://jmap.example.com/", client().get(&url))
        .await
        .expect_err("refused before sending");
    assert!(
        matches!(&failure, Failure::Source(err) if err.class() == FailureClass::Permanent),
        "{failure:?}"
    );
    assert!(matches!(&failure, Failure::Source(err) if err.is_credential_failure()));
    assert_eq!(*seen.lock().unwrap(), NONE, "nothing reached loopback");
    assert_eq!(source.asks(), 0);
}

#[tokio::test]
async fn an_ipv6_host_reaches_the_source_without_brackets() {
    let source = Arc::new(Tokens {
        permits: false,
        ..Tokens::new("A", &[])
    });
    let _ = get(&source, "http://[::1]:9/").await;
    assert_eq!(
        *source.checked.lock().unwrap(),
        [engine_provider::Origin::new("http", "::1", 9)]
    );
    assert_eq!(source.checked.lock().unwrap()[0].host(), "::1");
}
