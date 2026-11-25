//! Offline tests for what a dial tells the credential source and what it asks of it: the
//! protocol and origin each ask names, the dial deadline an ask runs under, a server the source
//! does not permit, a credential IMAP cannot present, and which failures are not refusals and so
//! ask for no replacement.

use std::sync::{Arc, atomic::Ordering};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject, Origin,
    ProviderError,
};

use super::{
    dial_with,
    renewal_tests::{
        GREETING, HOST, SASL_CAPABILITY, Servers, TokenSource, USER, accepting, refusing,
        token_config,
    },
};
use crate::{config::ImapConfig, credentials::PresentedIdentity, error::ImapError, mock::script};

#[tokio::test]
async fn every_ask_names_imap_and_the_servers_tls_name() {
    let source = TokenSource::new("expired-token", Some("refreshed-token"));
    let servers = Servers::new(vec![refusing(), accepting()]);
    // A loopback address mapped to a real name: the source is told the name.
    let config = ImapConfig::from_credential_source("127.0.0.1:993", HOST, source.clone());

    dial_with(&config, &PresentedIdentity::default(), || servers.open())
        .await
        .expect("the replacement authenticates");

    let origin = Origin::new("imaps", HOST, 993);
    assert_eq!(
        *source.asked.lock().unwrap(),
        [
            (CredentialProtocol::Imap, origin.clone()),
            (CredentialProtocol::Imap, origin),
        ]
    );
}

/// A source that is never heard from again: a hung token endpoint or broker.
#[derive(Debug)]
struct HungSource(CredentialSubject);

#[async_trait]
impl CredentialSource for HungSource {
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

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_fails_the_dial_as_retryable_at_the_dial_deadline() {
    let servers = Servers::new(vec![accepting()]);
    let config = token_config(Arc::new(HungSource(CredentialSubject::new("alice"))));
    let started = tokio::time::Instant::now();

    // Bounded twice over, so an unbounded ask fails here rather than hanging the suite.
    let err = tokio::time::timeout(
        2 * crate::deadline::BOUNDS.dial(),
        dial_with(&config, &PresentedIdentity::default(), || servers.open()),
    )
    .await
    .expect("the ask must give up by itself")
    .expect_err("no credential, no session");

    assert!(matches!(err, ImapError::Credential(_)), "{err:?}");
    assert_eq!(err.failure_class(), FailureClass::Retryable);
    assert_eq!(started.elapsed(), crate::deadline::BOUNDS.dial());
    assert_eq!(
        servers.opened(),
        0,
        "nothing is dialled without a credential"
    );
}

/// A source whose host does not let its credential reach this server.
#[derive(Debug)]
struct ElsewhereSource(CredentialSubject);

#[async_trait]
impl CredentialSource for ElsewhereSource {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    fn permits(&self, origin: &Origin) -> bool {
        origin.host() != HOST
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(Credential::bearer_for(USER, "a-token"))
    }
}

#[tokio::test]
async fn a_server_the_source_does_not_permit_is_never_dialled() {
    let servers = Servers::new(vec![accepting()]);
    let config = token_config(Arc::new(ElsewhereSource(CredentialSubject::new("alice"))));

    let err = dial_with(&config, &PresentedIdentity::default(), || servers.open())
        .await
        .expect_err("not permitted");

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert_eq!(
        servers.opened(),
        0,
        "nothing reaches a server not permitted"
    );
}

#[tokio::test]
async fn a_token_without_the_accounts_username_is_never_dialled() {
    let servers = Servers::new(vec![accepting()]);
    let config = ImapConfig::new(format!("{HOST}:993"), HOST, Credential::bearer("a-token"));

    let err = dial_with(&config, &PresentedIdentity::default(), || servers.open())
        .await
        .expect_err("SASL needs the username");

    assert_eq!(err.failure_class(), FailureClass::Permanent);
    assert_eq!(servers.opened(), 0);
}

#[tokio::test]
async fn a_failure_that_judged_no_credential_asks_for_no_replacement() {
    // A replacement re-dials and signs in again, which is only worth it when the server judged
    // the credential. None of these did: a limit on sessions, a temporary failure, a server
    // offering no mechanism a token can use.
    for (answer, class) in [
        (
            "a2 NO [LIMIT] too many sessions\r\n",
            FailureClass::RateLimited,
        ),
        (
            "a2 NO [UNAVAILABLE] directory down\r\n",
            FailureClass::Retryable,
        ),
    ] {
        let source = TokenSource::new("a-token", Some("another-token"));
        let servers = Servers::new(vec![
            script(&[GREETING, SASL_CAPABILITY, answer]),
            accepting(),
        ]);

        let err = dial_with(
            &token_config(source.clone()),
            &PresentedIdentity::default(),
            || servers.open(),
        )
        .await
        .expect_err("not signed in");

        assert_eq!(err.failure_class(), class, "{answer}");
        assert_eq!(source.renewals.load(Ordering::SeqCst), 0, "{answer}");
        assert_eq!(servers.opened(), 1, "{answer}");
    }

    let source = TokenSource::new("a-token", Some("another-token"));
    let password_only = "* CAPABILITY IMAP4rev1 AUTH=PLAIN\r\na1 OK CAPABILITY done\r\n";
    let servers = Servers::new(vec![script(&[GREETING, password_only]), accepting()]);
    let err = dial_with(
        &token_config(source.clone()),
        &PresentedIdentity::default(),
        || servers.open(),
    )
    .await
    .expect_err("no mechanism for a token");
    assert!(matches!(err, ImapError::AuthUnsupported(_)), "{err:?}");
    assert_eq!(err.failure_class(), FailureClass::Authentication);
    assert_eq!(source.renewals.load(Ordering::SeqCst), 0);
    assert_eq!(servers.opened(), 1);
}

/// A source that fails with `class`: a broker down, a grant revoked, a token endpoint
/// throttling.
#[derive(Debug)]
struct FailingSource(CredentialSubject, FailureClass);

#[async_trait]
impl CredentialSource for FailingSource {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Err(ProviderError::new(self.1, "the source failed"))
    }
}

/// Every way a dial can be without a credential reaches the provider's error marked as a
/// credential failure with the class it had, and no connection is opened for it; a dial the
/// connection failed is not marked, whatever its class.
#[tokio::test(start_paused = true)]
async fn a_dial_without_a_credential_says_so_and_a_lost_connection_does_not() {
    let subject = || CredentialSubject::new("alice");
    let without: Vec<(ImapConfig, FailureClass)> = vec![
        (
            token_config(Arc::new(FailingSource(subject(), FailureClass::Retryable))),
            FailureClass::Retryable,
        ),
        (
            token_config(Arc::new(FailingSource(
                subject(),
                FailureClass::Authentication,
            ))),
            FailureClass::Authentication,
        ),
        (
            token_config(Arc::new(HungSource(subject()))),
            FailureClass::Retryable,
        ),
        (
            token_config(Arc::new(ElsewhereSource(subject()))),
            FailureClass::Permanent,
        ),
        (
            ImapConfig::new(format!("{HOST}:993"), HOST, Credential::bearer("a-token")),
            FailureClass::Permanent,
        ),
    ];
    for (config, class) in without {
        let servers = Servers::new(vec![accepting()]);
        let err = dial_with(&config, &PresentedIdentity::default(), || servers.open())
            .await
            .expect_err("no credential, no session");
        let err = ProviderError::from(err);
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), class, "{err:?}");
        assert_eq!(servers.opened(), 0, "{err:?}");
    }

    // The server hung up before its greeting: the connection's failure, not the credential's.
    let servers = Servers::new(vec![Vec::new()]);
    let source = TokenSource::new("a-token", None);
    let err = dial_with(&token_config(source), &PresentedIdentity::default(), || {
        servers.open()
    })
    .await
    .expect_err("no greeting");
    let err = ProviderError::from(err);
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");
}
