//! Every way a CalDAV or CardDAV request can be without a credential reaches the host as a
//! credential failure ([`ProviderError::is_credential_failure`]), with the class it had and
//! nothing sent with a credential: the source failing or not answering, an origin the
//! credential may not go to (configured, or reached by discovery), and a kind HTTP cannot carry.
//! A failed exchange, a server refusing the credential, and a server asking for a method no
//! credential answers are not.

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, FixedCredential, Origin,
    ProviderError, ProviderResult,
};

use crate::{
    CalDavConfig, CalDavError, CalDavProvider, CardDavConfig, CardDavProvider,
    test_support::{mock_server_capturing, multistatus, redirect},
    transport::{DavClient, DavExecutor, DavMethod},
};

const EMPTY: &str = r#"<d:multistatus xmlns:d="DAV:"/>"#;
const CHALLENGE: &str = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"dav\"\r\nContent-Length: 0\r\n\
     Connection: close\r\n\r\n";

/// A source that fails each ask with its class.
#[derive(Debug)]
struct Failing(CredentialSubject, FailureClass);

#[async_trait]
impl CredentialSource for Failing {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        Err(ProviderError::new(self.1, "the broker said no"))
    }
}

/// A password source that permits only `host`.
#[derive(Debug)]
struct Only(CredentialSubject, &'static str);

#[async_trait]
impl CredentialSource for Only {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    fn permits(&self, origin: &Origin) -> bool {
        origin.host() == self.1
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        Ok(Credential::password("alice", "pw"))
    }
}

fn subject() -> CredentialSubject {
    CredentialSubject::new("acct-1")
}

fn password() -> Arc<dyn CredentialSource> {
    Arc::new(FixedCredential::unique(Credential::password("alice", "pw")))
}

/// What a `PROPFIND` the server challenges failed with, as a host sees it.
async fn propfind(base: &str, source: Arc<dyn CredentialSource>) -> CalDavError {
    DavClient::from_source(
        base,
        source,
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
    .send(DavMethod::Propfind, "/cal/", "0", String::new())
    .await
    .expect_err("no credential answers the challenge")
}

fn caldav(base: impl Into<String>, source: Arc<dyn CredentialSource>) -> CalDavConfig {
    CalDavConfig::from_credential_source(base, source, engine_http::test_server::user_agent())
}

/// The port of a mock server's `http://127.0.0.1:<port>` origin.
fn port(origin: &str) -> &str {
    origin.rsplit(':').next().expect("a port")
}

#[tokio::test]
async fn a_source_that_fails_is_a_credential_failure_with_its_class() {
    for class in [
        FailureClass::Retryable,
        FailureClass::Authentication,
        FailureClass::RateLimited,
    ] {
        let (base, seen) = mock_server_capturing(vec![CHALLENGE.to_owned()]);
        let err = propfind(&base, Arc::new(Failing(subject(), class))).await;
        let err = ProviderError::from(err);
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), class);
        assert_eq!(seen.lock().unwrap().len(), 1, "only the anonymous request");

        // The import and collection paths, which keep the source's words out, keep the fact.
        let (base, _) = mock_server_capturing(vec![CHALLENGE.to_owned()]);
        let err = propfind(&base, Arc::new(Failing(subject(), class))).await;
        let content_free = err.content_free("looking up a UID");
        assert!(content_free.is_credential_failure(), "{content_free:?}");
        assert_eq!(content_free.class(), class);
    }
}

/// A source that answers once and is never heard from again: a token endpoint that hung.
#[derive(Debug)]
struct AnswersOnce(CredentialSubject, std::sync::atomic::AtomicBool);

#[async_trait]
impl CredentialSource for AnswersOnce {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        if self.1.swap(true, std::sync::atomic::Ordering::SeqCst) {
            std::future::pending().await
        } else {
            Ok(Credential::password("alice", "pw"))
        }
    }
}

#[tokio::test]
async fn a_source_that_never_answers_is_a_credential_failure_at_the_deadline() {
    let (base, _) = mock_server_capturing(vec![CHALLENGE.to_owned(), multistatus(EMPTY)]);
    let source = AnswersOnce(subject(), std::sync::atomic::AtomicBool::new(false));
    let client = DavClient::from_source(
        &base,
        Arc::new(source),
        &engine_tls::TlsClientConfig::default(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client");
    let propfind = || client.send(DavMethod::Propfind, "/cal/", "0", String::new());
    assert_eq!(propfind().await.expect("answered").status, 207);

    // The origin has asked, so the next request asks the source before anything is sent, and
    // only the ask's deadline can end it: the clock is paused from here, with nothing on the
    // wire to wait for.
    tokio::time::pause();
    let err = ProviderError::from(propfind().await.expect_err("no answer"));
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn an_origin_or_a_kind_the_credential_may_not_go_to_is_a_credential_failure() {
    // A configured server the source does not permit, and one on plaintext off loopback
    // (TEST-NET-1, never reached).
    let (base, seen) = mock_server_capturing(vec![multistatus(EMPTY); 4]);
    let refused = [
        caldav(&base, Arc::new(Only(subject(), "localhost"))),
        caldav("http://192.0.2.1:9/", password()),
    ];
    for config in refused {
        let err = ProviderError::from(CalDavProvider::connect(config).await.expect_err("refused"));
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), FailureClass::Permanent);
    }
    assert!(seen.lock().unwrap().is_empty());

    // Discovery redirected to a host the source does not permit, on CalDAV and on CardDAV.
    let (moved, moved_seen) = mock_server_capturing(vec![multistatus(EMPTY); 8]);
    let moved = format!("http://localhost:{}/dav/", port(&moved));
    let only_loopback = || Arc::new(Only(subject(), "127.0.0.1"));
    let (base, _) = mock_server_capturing(vec![redirect(&moved)]);
    let err = CalDavProvider::connect(caldav(&base, only_loopback()))
        .await
        .expect_err("refused");
    assert!(ProviderError::from(err).is_credential_failure());
    let (base, _) = mock_server_capturing(vec![redirect(&moved)]);
    let config = CardDavConfig::from_credential_source(
        base,
        only_loopback(),
        engine_http::test_server::user_agent(),
    );
    let err = CardDavProvider::connect(config).await.expect_err("refused");
    assert!(ProviderError::from(err).is_credential_failure());
    assert!(moved_seen.lock().unwrap().is_empty());

    // A kind HTTP cannot carry.
    let (base, _) = mock_server_capturing(vec![CHALLENGE.to_owned()]);
    let acting_as = Credential::acting_as("admin", "pw", "alice");
    let err = propfind(&base, Arc::new(FixedCredential::unique(acting_as))).await;
    assert!(ProviderError::from(err).is_credential_failure());
}

#[tokio::test]
async fn a_failed_exchange_or_a_server_s_answer_is_not_a_credential_failure() {
    // Nothing listens on the discard port.
    let err = ProviderError::from(propfind("http://127.0.0.1:9/", password()).await);
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");

    // A challenge only a method no credential carries answers.
    let digest = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"dav\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, _) = mock_server_capturing(vec![digest.to_owned()]);
    let err = ProviderError::from(propfind(&base, password()).await);
    assert!(!err.is_credential_failure(), "{err:?}");

    // The password refused at connect, and no replacement.
    let (base, _) = mock_server_capturing(vec![CHALLENGE.to_owned(); 4]);
    let err = CalDavProvider::connect(caldav(&base, password()))
        .await
        .expect_err("refused");
    let err = ProviderError::from(err);
    assert_eq!(err.class(), FailureClass::Authentication, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");
}

/// An object without a credential ends the batch whatever the source's class, so a source that is
/// down or hung is not asked again, at a deadline each, for every later object.
#[tokio::test]
async fn an_object_without_a_credential_ends_the_import_batch() {
    let origin = Origin::new("https", "dav.example.com", 443);
    let request = CredentialRequest::new(engine_provider::CredentialProtocol::Http, &origin);
    let source = Failing(subject(), FailureClass::Retryable);
    let attempt = |_| async { Ok::<(), ProviderError>(()) };
    let limit = std::time::Duration::from_secs(1);
    let answer =
        engine_provider::with_credential(&source, &request, limit, |_| false, attempt).await;
    assert!(crate::dav_import::ending(&answer).is_some());
    let transport: ProviderResult<()> = Err(ProviderError::retryable("connection reset"));
    assert!(crate::dav_import::ending(&transport).is_none());
}
