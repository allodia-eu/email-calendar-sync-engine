//! Every way a Graph request can be without a credential reaches the host as a credential failure
//! ([`ProviderError::is_credential_failure`]), with the class it had and nothing sent; a failed
//! exchange and Graph refusing the token are not.

use std::sync::Arc;

use async_trait::async_trait;
use engine_core::error::FailureClass;
use engine_http::test_server::scripted;
use engine_provider::{
    Credential, CredentialRequest, CredentialSource, CredentialSubject, FixedCredential, HandOver,
    Origin, ProviderError, Unrecorded,
};

use crate::{
    GraphClient, GraphError,
    test_support::{retry, tls, user_agent},
};

/// A source that fails each ask with its class, never answers, or answers a token and permits
/// only one host.
#[derive(Debug)]
enum Source {
    Fails(CredentialSubject, FailureClass),
    Hangs(CredentialSubject),
    Only(CredentialSubject, &'static str),
}

#[async_trait]
impl CredentialSource for Source {
    fn subject(&self) -> &CredentialSubject {
        match self {
            Self::Fails(subject, _) | Self::Hangs(subject) | Self::Only(subject, _) => subject,
        }
    }

    fn permits(&self, origin: &Origin) -> bool {
        match self {
            Self::Only(_, host) => origin.host() == *host,
            _ => true,
        }
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        match self {
            Self::Fails(_, class) => Err(ProviderError::new(*class, "the broker said no")),
            Self::Hangs(_) => std::future::pending().await,
            Self::Only(..) => Ok(Credential::bearer("tok-a")),
        }
    }
}

fn subject() -> CredentialSubject {
    CredentialSubject::new("acct-1")
}

fn client(base: &str, source: Arc<dyn CredentialSource>) -> GraphClient {
    GraphClient::with_base_and_source(source, base, tls(), &retry(), &user_agent()).expect("client")
}

/// What a read of the folders failed with, as a host sees it.
async fn read(base: &str, source: Arc<dyn CredentialSource>) -> ProviderError {
    let client = client(base, source);
    client
        .get(&client.url("/mailFolders"))
        .await
        .expect_err("no folders")
        .into()
}

fn reply(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_source_that_fails_is_a_credential_failure_with_its_class_on_a_read_and_a_send() {
    for class in [
        FailureClass::Retryable,
        FailureClass::Authentication,
        FailureClass::RateLimited,
    ] {
        let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
        let err = read(&base, Arc::new(Source::Fails(subject(), class))).await;
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), class);

        let client = client(&base, Arc::new(Source::Fails(subject(), class)));
        let err: GraphError = client
            .submit(
                &client.url("/sendMail"),
                "text/plain",
                b"bWltZQ==".to_vec(),
                &HandOver::new(&Unrecorded),
            )
            .await
            .expect_err("not sent");
        let err = err.into_submission_error();
        assert!(err.is_credential_failure(), "{err:?}");
        assert!(!err.requires_confirmation(), "nothing was sent");
        assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
    }
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_is_a_credential_failure_at_the_deadline() {
    let (base, _) = scripted(vec![reply("200 OK", "{}")]);
    let err = read(&base, Arc::new(Source::Hangs(subject()))).await;
    assert!(err.is_credential_failure(), "{err:?}");
    assert_eq!(err.class(), FailureClass::Retryable);
}

#[tokio::test]
async fn an_origin_or_a_kind_the_credential_may_not_go_to_is_a_credential_failure() {
    let (base, seen) = scripted(vec![reply("200 OK", "{}")]);
    let not_permitted = read(
        &base,
        Arc::new(Source::Only(subject(), "graph.example.com")),
    )
    .await;
    let password = Credential::password("alice", "secret");
    let a_password = read(&base, Arc::new(FixedCredential::unique(password))).await;
    // An account on the real root, handed an `http://` link to a loopback server.
    let token = Arc::new(FixedCredential::unique(Credential::bearer("tok-a")));
    let plaintext = client("https://graph.example.com/v1.0", token)
        .get(&format!("{base}/v1.0/me"))
        .await
        .expect_err("refused")
        .into();
    for err in [not_permitted, a_password, plaintext] {
        let err: ProviderError = err;
        assert!(err.is_credential_failure(), "{err:?}");
        assert_eq!(err.class(), FailureClass::Permanent);
    }
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
}

#[tokio::test]
async fn a_failed_exchange_or_a_refused_token_is_not_a_credential_failure() {
    let token = || Arc::new(FixedCredential::unique(Credential::bearer("tok-a")));
    // The script's one reply closes the connection unanswered.
    let (base, _) = scripted(vec![String::new()]);
    let err = read(&base, token()).await;
    assert_eq!(err.class(), FailureClass::Retryable, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");

    let refused = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"\"\r\n\
                   Content-Length: 0\r\nConnection: close\r\n\r\n";
    let (base, _) = scripted(vec![refused.to_owned()]);
    let err = read(&base, token()).await;
    assert_eq!(err.class(), FailureClass::Authentication, "{err:?}");
    assert!(!err.is_credential_failure(), "{err:?}");
}
