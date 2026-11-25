//! [`with_credential`]: one ask, at most one replacement, the second refusal as it came, the
//! source's classes passed through, and a source that never answers bounded.

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use async_trait::async_trait;
use engine_core::error::FailureClass;

use super::{
    Credential, CredentialFailure, CredentialProtocol, CredentialRequest, CredentialSource,
    CredentialSubject, FixedCredential, Origin, with_credential,
};
use crate::{CredentialFailureKind, ProviderError};

const LIMIT: Duration = Duration::from_secs(30);

/// What a test attempt can fail with: the server refusing the token it was given, some other
/// failure, or the source's own.
#[derive(Debug)]
enum Failure {
    Refused(String),
    Broken,
    Source(ProviderError),
}

impl From<CredentialFailure> for Failure {
    fn from(failure: CredentialFailure) -> Self {
        Self::Source(failure.into_error())
    }
}

fn is_refusal(failure: &Failure) -> bool {
    matches!(failure, Failure::Refused(_))
}

/// How a scripted source answers one ask.
#[derive(Debug)]
enum Answer {
    Token(&'static str),
    NoReplacement,
    Fails(FailureClass),
    Never,
}

/// A token source that answers from a script and records every ask.
#[derive(Debug)]
struct Scripted {
    subject: CredentialSubject,
    permits: bool,
    answers: Mutex<VecDeque<Answer>>,
    asks: Mutex<Vec<(&'static str, CredentialProtocol, Origin)>>,
    refused: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            subject: CredentialSubject::new("acct-1"),
            permits: true,
            answers: Mutex::new(answers.into()),
            asks: Mutex::default(),
            refused: Mutex::default(),
        }
    }

    fn asks(&self, kind: &str) -> usize {
        self.asks
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, ..)| *k == kind)
            .count()
    }

    async fn answer(
        &self,
        kind: &'static str,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        self.asks
            .lock()
            .unwrap()
            .push((kind, request.protocol, request.origin.clone()));
        let next = self.answers.lock().unwrap().pop_front();
        match next.expect("the source was asked more often than scripted") {
            Answer::Token(token) => Ok(Some(Credential::bearer_for("alice@example.com", token))),
            Answer::NoReplacement => Ok(None),
            Answer::Fails(class) => Err(ProviderError::new(class, "the source failed")),
            Answer::Never => core::future::pending().await,
        }
    }
}

#[async_trait]
impl CredentialSource for Scripted {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    fn permits(&self, _origin: &Origin) -> bool {
        self.permits
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(self
            .answer("credential", request)
            .await?
            .expect("a credential, not none"))
    }

    async fn replace(
        &self,
        refused: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        self.refused.lock().unwrap().push(token_of(refused));
        self.answer("replace", request).await
    }
}

fn token_of(credential: &Credential) -> String {
    match credential {
        Credential::Bearer { token, .. } => token.expose().to_owned(),
        Credential::Password { password, .. } | Credential::ActingAs { password, .. } => {
            password.expose().to_owned()
        }
    }
}

/// A server that accepts only `good`, and records the token of every attempt.
struct Server {
    good: &'static str,
    presented: Mutex<Vec<String>>,
    broken: bool,
}

impl Server {
    fn accepting(good: &'static str) -> Self {
        Self {
            good,
            presented: Mutex::default(),
            broken: false,
        }
    }

    fn attempt(&self, credential: &Credential) -> core::future::Ready<Result<String, Failure>> {
        let token = token_of(credential);
        self.presented.lock().unwrap().push(token.clone());
        core::future::ready(if self.broken {
            Err(Failure::Broken)
        } else if token == self.good {
            Ok(token)
        } else {
            Err(Failure::Refused(token))
        })
    }

    fn presented(&self) -> Vec<String> {
        self.presented.lock().unwrap().clone()
    }
}

async fn run(source: &dyn CredentialSource, server: &Server) -> Result<String, Failure> {
    let origin = Origin::new("imaps", "imap.example.com", 993);
    let request = CredentialRequest::new(CredentialProtocol::Imap, &origin);
    with_credential(source, &request, LIMIT, is_refusal, |c| server.attempt(&c)).await
}

#[tokio::test]
async fn an_accepted_credential_is_asked_for_once_and_never_replaced() {
    let source = Scripted::new(vec![Answer::Token("good")]);
    let server = Server::accepting("good");

    assert_eq!(run(&source, &server).await.expect("accepted"), "good");
    assert_eq!((source.asks("credential"), source.asks("replace")), (1, 0));
    assert_eq!(server.presented(), ["good"]);
}

#[tokio::test]
async fn a_refused_credential_is_replaced_once_and_the_replacement_presented() {
    let source = Scripted::new(vec![Answer::Token("expired"), Answer::Token("fresh")]);
    let server = Server::accepting("fresh");

    assert_eq!(run(&source, &server).await.expect("replaced"), "fresh");
    assert_eq!((source.asks("credential"), source.asks("replace")), (1, 1));
    assert_eq!(server.presented(), ["expired", "fresh"]);
    // The source is told which credential the server refused.
    assert_eq!(*source.refused.lock().unwrap(), ["expired"]);
}

#[tokio::test]
async fn both_asks_name_the_protocol_and_origin_of_the_request() {
    let source = Scripted::new(vec![Answer::Token("expired"), Answer::Token("fresh")]);
    run(&source, &Server::accepting("fresh"))
        .await
        .expect("replaced");

    let origin = Origin::new("imaps", "imap.example.com", 993);
    let asks = source.asks.lock().unwrap().clone();
    assert_eq!(
        asks,
        [
            ("credential", CredentialProtocol::Imap, origin.clone()),
            ("replace", CredentialProtocol::Imap, origin),
        ]
    );
}

#[tokio::test]
async fn a_replacement_refused_again_is_returned_as_it_came() {
    let source = Scripted::new(vec![Answer::Token("expired"), Answer::Token("also-bad")]);
    let server = Server::accepting("good");

    let err = run(&source, &server).await.expect_err("two refusals");
    assert!(
        matches!(&err, Failure::Refused(t) if t == "also-bad"),
        "{err:?}"
    );
    assert_eq!(source.asks("replace"), 1, "at most one replacement");
    assert_eq!(server.presented(), ["expired", "also-bad"]);
}

#[tokio::test]
async fn without_a_replacement_the_first_refusal_is_returned() {
    let source = Scripted::new(vec![Answer::Token("expired"), Answer::NoReplacement]);
    let server = Server::accepting("good");

    let err = run(&source, &server).await.expect_err("no replacement");
    assert!(
        matches!(&err, Failure::Refused(t) if t == "expired"),
        "{err:?}"
    );
    assert_eq!(server.presented(), ["expired"]);
}

#[tokio::test]
async fn a_failure_that_is_not_a_refusal_asks_for_no_replacement() {
    let source = Scripted::new(vec![Answer::Token("good")]);
    let server = Server {
        broken: true,
        ..Server::accepting("good")
    };

    let err = run(&source, &server).await.expect_err("broken");
    assert!(matches!(err, Failure::Broken), "{err:?}");
    assert_eq!(source.asks("replace"), 0);
}

#[tokio::test]
async fn a_fixed_credential_is_never_replaced() {
    let source = FixedCredential::unique(Credential::password("alice@example.com", "wrong"));
    let server = Server::accepting("right");

    let err = run(&source, &server).await.expect_err("refused");
    assert!(matches!(err, Failure::Refused(_)), "{err:?}");
    assert_eq!(
        server.presented(),
        ["wrong"],
        "the same password is never sent twice"
    );
}

#[tokio::test]
async fn the_sources_failures_keep_their_class() {
    for class in [
        FailureClass::Authentication,
        FailureClass::Retryable,
        FailureClass::RateLimited,
        FailureClass::Permanent,
    ] {
        // Failing the first ask: nothing is attempted.
        let source = Scripted::new(vec![Answer::Fails(class)]);
        let server = Server::accepting("good");
        let err = run(&source, &server).await.expect_err("no credential");
        assert!(
            matches!(&err, Failure::Source(e) if e.class() == class && e.credential_failure() == Some(CredentialFailureKind::Source)),
            "{err:?}"
        );
        assert_eq!(server.presented(), Vec::<String>::new());

        // Failing the replacement: the source's failure, not the refusal.
        let source = Scripted::new(vec![Answer::Token("expired"), Answer::Fails(class)]);
        let err = run(&source, &Server::accepting("good"))
            .await
            .expect_err("no replacement");
        assert!(
            matches!(&err, Failure::Source(e) if e.class() == class && e.credential_failure() == Some(CredentialFailureKind::Source)),
            "{err:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_fails_as_retryable_once_the_limit_passes() {
    for script in [
        vec![Answer::Never],
        vec![Answer::Token("expired"), Answer::Never],
    ] {
        let source = Scripted::new(script);
        let server = Server::accepting("good");
        let started = tokio::time::Instant::now();

        // Bounded twice over, so an ask without a limit fails here rather than hanging.
        let err = tokio::time::timeout(LIMIT * 2, run(&source, &server))
            .await
            .expect("the ask must give up by itself")
            .expect_err("no answer");

        assert!(
            matches!(
                &err,
                Failure::Source(e)
                    if e.class() == FailureClass::Retryable
                        && e.credential_failure() == Some(CredentialFailureKind::Source)
            ),
            "{err:?}"
        );
        assert_eq!(
            started.elapsed(),
            LIMIT,
            "bounded by the limit, and only by it"
        );
    }
}

#[tokio::test]
async fn an_origin_the_source_does_not_permit_is_sent_nothing() {
    let source = Scripted {
        permits: false,
        ..Scripted::new(vec![Answer::Token("good")])
    };
    let server = Server::accepting("good");

    let err = run(&source, &server).await.expect_err("refused origin");

    assert!(
        matches!(
            &err,
            Failure::Source(e) if e.class() == FailureClass::Permanent
                && e.credential_failure() == Some(CredentialFailureKind::NotPermitted)
        ),
        "{err:?}"
    );
    assert_eq!(source.asks("credential"), 0, "the source is not even asked");
    assert_eq!(
        server.presented(),
        Vec::<String>::new(),
        "nothing reaches the origin"
    );
}

#[test]
fn a_source_failure_converts_into_the_provider_error_it_carries() {
    let failure = CredentialFailure::new(ProviderError::authentication("consent is gone"));
    assert_eq!(
        failure.to_string(),
        ProviderError::authentication("consent is gone").to_string()
    );
    assert!(std::error::Error::source(&failure).is_some());
    let error: ProviderError = failure.into();
    assert_eq!(error.class(), FailureClass::Authentication);
    assert_eq!(
        error.credential_failure(),
        Some(CredentialFailureKind::Source)
    );
}

#[test]
fn an_origin_the_engine_refuses_is_a_permanent_failure_with_its_reason() {
    let failure = CredentialFailure::refused_origin("plaintext to a remote host");
    let error = failure.into_error();
    assert_eq!(error.class(), FailureClass::Permanent);
    assert_eq!(
        error.credential_failure(),
        Some(CredentialFailureKind::Insecure)
    );
    assert!(
        error.to_string().contains("plaintext to a remote host"),
        "{error}"
    );
}

#[test]
fn a_kind_the_adapter_cannot_present_is_a_permanent_failure_with_its_reason() {
    let failure = CredentialFailure::unpresentable("a password to a token-only API");
    let error = failure.into_error();
    assert_eq!(error.class(), FailureClass::Permanent);
    assert_eq!(
        error.credential_failure(),
        Some(CredentialFailureKind::Form)
    );
    assert!(
        error.to_string().contains("a password to a token-only API"),
        "{error}"
    );
}

#[test]
fn a_credential_for_another_account_is_invalid_state_with_its_reason() {
    let error = CredentialFailure::another_account("signed in as another account").into_error();
    assert_eq!(error.class(), FailureClass::InvalidState);
    assert_eq!(
        error.credential_failure(),
        Some(CredentialFailureKind::Account)
    );
    assert!(
        error.to_string().contains("signed in as another account"),
        "{error}"
    );
}

/// A source's rate limit keeps its instant through the failure, as every other fact it gave.
#[tokio::test]
async fn a_throttled_source_keeps_its_wait() {
    #[derive(Debug)]
    struct Throttled(CredentialSubject);

    #[async_trait]
    impl CredentialSource for Throttled {
        fn subject(&self) -> &CredentialSubject {
            &self.0
        }

        async fn credential(
            &self,
            _request: &CredentialRequest<'_>,
        ) -> Result<Credential, ProviderError> {
            let wait = "PT30S".parse().ok();
            Err(ProviderError::rate_limited("token endpoint 429", wait))
        }
    }

    let source = Throttled(CredentialSubject::new("acct-1"));
    let err = run(&source, &Server::accepting("good"))
        .await
        .expect_err("throttled");
    let Failure::Source(error) = err else {
        panic!("{err:?}")
    };
    assert_eq!(error.retry_after(), "PT30S".parse().ok());
    assert_eq!(
        error.credential_failure(),
        Some(CredentialFailureKind::Source)
    );
}

/// A source's error keeps its class, words and wait, and nothing that speaks of a request that
/// went out: none did.
#[test]
fn a_source_s_failure_says_nothing_of_a_request_that_went_out() {
    for answered in [
        ProviderError::needs_confirmation("the broker lost its answer"),
        ProviderError::item_unavailable("the broker has no such mailbox"),
        ProviderError::refused_by_target(crate::TargetRefusal::StorageFull),
    ] {
        let class = answered.class();
        let error = CredentialFailure::new(answered).into_error();
        assert_eq!(
            error.credential_failure(),
            Some(CredentialFailureKind::Source)
        );
        assert_eq!(error.class(), class);
        assert!(!error.requires_confirmation(), "{error:?}");
        assert!(!error.is_item_unavailable(), "{error:?}");
        assert_eq!(error.target_refusal(), None, "{error:?}");
    }
}
