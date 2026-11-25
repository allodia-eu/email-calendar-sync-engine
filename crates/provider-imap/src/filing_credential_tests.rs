//! Offline tests for what a submission tells the credential source and what it asks of it: the
//! protocol and origin each ask names, which `AUTH` failures are not refusals and so ask for no
//! replacement, and the submissions that never dial: a server the source does not permit, and a
//! credential SMTP cannot present.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_provider::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject,
    HandOver, Origin, ProviderError, Unrecorded,
};
use tokio::net::TcpListener;

use super::{
    resolve_smtp,
    smtp_server_tests::{
        Tokens, cert_and_acceptor, draft, oauth_server, provider_with_smtp, token_provider,
        trusting_connector,
    },
};
use crate::config::ImapConfig;

#[tokio::test]
async fn every_ask_names_smtp_and_the_submission_servers_tls_name() {
    let (cert, port, _auths) =
        oauth_server(vec!["535 5.7.8 token expired\r\n", "235 2.7.0 ok\r\n"]).await;
    let source = Tokens::new(&["expired-token"], Some("refreshed-token"));
    let asked = Arc::clone(&source.asked);
    let provider = token_provider(cert, port, source);

    provider
        .submit(
            &draft(),
            &engine_provider::HandOver::new(&engine_provider::Unrecorded),
        )
        .await
        .expect("the replacement sends");

    // The submission server, not the IMAP one the account reads from.
    let origin = Origin::new("submissions", "127.0.0.1", port);
    assert_eq!(
        *asked.lock().unwrap(),
        [
            (CredentialProtocol::Smtp, origin.clone()),
            (CredentialProtocol::Smtp, origin),
        ]
    );
}

#[tokio::test]
async fn an_auth_failure_that_judged_no_credential_asks_for_no_replacement() {
    // Only `535` judged the credential. A deferral and an unsupported mechanism did not, so a
    // replacement would re-dial for the same answer.
    for (reply, class) in [
        (
            "454 4.7.0 Temporary authentication failure\r\n",
            FailureClass::RateLimited,
        ),
        (
            "504 5.5.4 Unrecognized authentication type\r\n",
            FailureClass::Authentication,
        ),
    ] {
        let (cert, port, auths) = oauth_server(vec![reply, "235 2.7.0 ok\r\n"]).await;
        let source = Tokens::new(&["a-token"], Some("another-token"));
        let asked = Arc::clone(&source.asked);
        let provider = token_provider(cert, port, source);

        let err = provider
            .submit(&draft(), &HandOver::new(&Unrecorded))
            .await
            .expect_err("not signed in");

        assert_eq!(err.class(), class, "{reply}");
        assert_eq!(asked.lock().unwrap().len(), 1, "no replacement: {reply}");
        assert_eq!(auths.lock().unwrap().len(), 1, "one AUTH: {reply}");
    }
}

/// A listener that counts the connections made to it and closes each at once.
async fn counting_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((connection, _)) = listener.accept().await {
            // Counted before it closes, so a client that saw it close sees the count.
            counter.fetch_add(1, Ordering::SeqCst);
            drop(connection);
        }
    });
    (port, accepted)
}

/// A submission over implicit TLS to `port`, with its credential from `source`.
async fn submit_to(port: u16, source: Arc<dyn CredentialSource>) -> ProviderError {
    let config = ImapConfig::from_credential_source("h:993", "127.0.0.1", source)
        .with_smtp_tls(format!("127.0.0.1:{port}"), "127.0.0.1");
    let (cert, _) = cert_and_acceptor();
    let sender = resolve_smtp(
        config.smtp.as_ref().unwrap(),
        &trusting_connector(cert),
        &config,
    );
    provider_with_smtp(sender)
        .submit(&draft(), &HandOver::new(&Unrecorded))
        .await
        .expect_err("no submission")
}

/// A source whose host does not let its credential reach the loopback submission server.
#[derive(Debug)]
struct Elsewhere(CredentialSubject);

#[async_trait]
impl CredentialSource for Elsewhere {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    fn permits(&self, origin: &Origin) -> bool {
        origin.host() != "127.0.0.1"
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Ok(Credential::bearer_for("alice@test.local", "a-token"))
    }
}

#[tokio::test]
async fn a_submission_server_the_source_does_not_permit_is_never_dialled() {
    let (port, accepted) = counting_listener().await;

    let err = submit_to(port, Arc::new(Elsewhere(CredentialSubject::new("alice")))).await;

    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "nothing reaches it");
}

#[tokio::test]
async fn a_token_without_the_accounts_username_is_never_submitted() {
    let (port, accepted) = counting_listener().await;
    let source = engine_provider::FixedCredential::unique(Credential::bearer("a-token"));

    let err = submit_to(port, Arc::new(source)).await;

    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "nothing is dialled");
}

#[tokio::test]
async fn a_credential_acting_as_another_account_is_never_submitted() {
    // Submitting as another account is not the grant an administrator's access to a mailbox
    // is, so the submission refuses before it dials, and the password goes nowhere.
    let (port, accepted) = counting_listener().await;
    let credential = Credential::acting_as("master@example.com", "master-pw", "alice@example.com");
    let source = engine_provider::FixedCredential::unique(credential);

    let err = submit_to(port, Arc::new(source)).await;

    assert_eq!(err.class(), FailureClass::Permanent);
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "nothing is dialled");
}

/// A source whose token endpoint cannot be reached.
#[derive(Debug)]
struct Unreachable(CredentialSubject);

#[async_trait]
impl CredentialSource for Unreachable {
    fn subject(&self) -> &CredentialSubject {
        &self.0
    }

    async fn credential(
        &self,
        _request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        Err(ProviderError::retryable(
            "the token endpoint cannot be reached",
        ))
    }
}

#[tokio::test]
async fn a_submission_without_a_credential_says_so_and_a_closed_connection_does_not() {
    let (port, accepted) = counting_listener().await;
    let err = submit_to(port, Arc::new(Unreachable(CredentialSubject::new("alice")))).await;
    assert_eq!(err.class(), FailureClass::Retryable);
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "nothing is dialled");

    // The server closes the connection at once: the connection's failure, not the credential's.
    let (port, accepted) = counting_listener().await;
    let credential = Credential::bearer_for("alice@test.local", "a-token");
    let source = engine_provider::FixedCredential::unique(credential);
    let err = submit_to(port, Arc::new(source)).await;
    assert!(!err.is_credential_failure(), "{err:?}");
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
}
