//! Every way a JMAP request can be without a credential reaches the host as a credential failure
//! ([`ProviderError::is_credential_failure`]), with the class it had and nothing sent; a failed
//! exchange and a server refusing the credential are not.

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, Origin, ProviderError,
};

use super::{
    Transport,
    credential_tests::{BEARER_CHALLENGE, Rotating, config, connect_error, transport},
    transport_tests::{mock_server, ok, tls, unauthorized},
};
use crate::credential_test_support::Failing;

/// A source that is never heard from again: a hung token endpoint or broker.
#[derive(Debug)]
struct Hung(CredentialSubject);

#[async_trait]
impl CredentialSource for Hung {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        std::future::pending().await
    }
}

/// A source that gives a token and then cannot replace it: its token endpoint went down.
#[derive(Debug)]
struct NoReplacement(CredentialSubject);

#[async_trait]
impl CredentialSource for NoReplacement {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(Credential::bearer("expired"))
    }

    async fn replace(
        &self,
        _refused: &Credential,
        _request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        Err(ProviderError::retryable("the token endpoint is down"))
    }
}

fn over(source: Arc<dyn CredentialSource>) -> Transport {
    Transport::new(
        &reqwest::Url::parse("http://127.0.0.1/").unwrap(),
        source,
        tls(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .unwrap()
}

/// What a request on `source` failed with, as a host sees it.
async fn failed(source: Arc<dyn CredentialSource>, url: &str) -> ProviderError {
    match over(source).get(url).await {
        Ok(_) => panic!("the request should fail"),
        Err(err) => err.into(),
    }
}

fn subject() -> CredentialSubject {
    CredentialSubject::new("acct-1")
}

#[tokio::test]
async fn a_source_that_fails_is_a_credential_failure_with_its_class_and_nothing_is_sent() {
    for class in [
        FailureClass::Retryable,
        FailureClass::Authentication,
        FailureClass::RateLimited,
    ] {
        let (base, seen) = mock_server(vec![ok("{}")]);
        let err = failed(Arc::new(Failing(subject(), class)), &base).await;
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), class);
        assert_eq!(seen.lock().unwrap().len(), 0, "nothing was sent");
    }

    // Refused, then the source could not replace it: the source's failure, in place of the
    // refusal, and the server saw only the refused request.
    let (base, seen) = mock_server(vec![unauthorized(BEARER_CHALLENGE)]);
    let err = failed(Arc::new(NoReplacement(subject())), &base).await;
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Retryable);
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_is_a_credential_failure_at_the_deadline() {
    let err = failed(Arc::new(Hung(subject())), "http://127.0.0.1:9/").await;
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn an_origin_or_a_kind_the_credential_may_not_go_to_is_a_credential_failure() {
    // Plaintext off loopback, configured.
    let source = Arc::new(Rotating::tokens("a", &[]));
    let err = connect_error(config("http://jmap.example.com", &source)).await;
    assert!(err.is_credential_failure(), "{err:?}");

    // An origin the source does not permit.
    let (base, _) = mock_server(vec![ok("{}")]);
    let mut source = Rotating::tokens("a", &[]);
    source.permitted = Some(vec![Origin::new("https", "jmap.example.com", 443)]);
    let err = connect_error(config(&base, &Arc::new(source))).await;
    assert!(err.is_credential_failure(), "{err:?}");

    // A kind JMAP cannot present.
    let (base, seen) = mock_server(vec![ok("{}")]);
    let source = Arc::new(Rotating::new(
        Credential::acting_as("admin@example.com", "s3cret", "alice@example.com"),
        Vec::new(),
    ));
    let Err(err) = transport(&source).get(&base).await else {
        panic!("refused")
    };
    let err = ProviderError::from(err);
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Permanent);
    assert_eq!(seen.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn a_failed_exchange_or_a_refused_credential_is_not_a_credential_failure() {
    // Nothing listens on the discard port: the connection is refused.
    let source = Arc::new(Rotating::tokens("a", &[]));
    let err = failed(source, "http://127.0.0.1:9/").await;
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");

    // The server refused the token, and the source had none other.
    let (base, _) = mock_server(vec![unauthorized(BEARER_CHALLENGE)]);
    let source = Arc::new(Rotating::tokens("a", &[]));
    let err = connect_error(config(&base, &source)).await;
    assert_eq!(err.class(), FailureClass::Authentication, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");
}

/// A token holding a byte a header cannot carry, and a password framed under Basic whose
/// username holds a colon, are refused before anything is sent, as credential failures that no
/// retry can mend, and the error repeats neither.
#[tokio::test]
async fn a_credential_a_header_cannot_carry_is_a_credential_failure_and_nothing_is_sent() {
    for credential in [
        Credential::bearer("secret\r\nX-Injected: 1"),
        Credential::bearer("sécret"),
    ] {
        let (base, seen) = mock_server(vec![ok("{}")]);
        let source = Arc::new(Rotating::new(credential, Vec::new()));
        let Err(err) = transport(&source).get(&base).await else {
            panic!("refused")
        };
        let err = ProviderError::from(err);
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(!format!("{err:?}").contains("cret"), "{err:?}");
        assert_eq!(seen.lock().unwrap().len(), 0, "nothing was sent");
    }
}
