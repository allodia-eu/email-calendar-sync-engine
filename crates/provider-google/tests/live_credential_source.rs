//! Gated live proof that a Gmail account asks one credential source per request, on both of its
//! transports: a token the Gmail API refuses is replaced once and the request succeeds with the
//! replacement, and the account's IMAP half asks the same source, so a token replaced after
//! either half's refusal is the one the other presents next.
//!
//! Read-only: it lists the labels and opens an IMAP session. The first token presented is a
//! garbage one Google cannot have issued; the replacement is `GOOGLE_ACCESS_TOKEN`. Nothing is
//! printed but counts.
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_credential_source -- --nocapture --test-threads=1
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject,
    Provider, ProviderError,
};
use provider_google::{GmailProvider, GoogleClient};
use provider_imap::{GmailMail, ImapAccount, ImapConfig};
use tokio::io::{AsyncRead, AsyncWrite};

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A token Google cannot have issued.
const GARBAGE: &str = "ya29.garbage-token-for-a-live-test";

/// What a source was asked, by protocol: credentials, then replacements.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Asked {
    http: (usize, usize),
    imap: (usize, usize),
}

/// Answers `first` until asked to replace it, and `real` from then on, as a refreshing source
/// does, always with the account's address, which IMAP's SASL needs and the API ignores.
#[derive(Debug)]
struct Source {
    subject: CredentialSubject,
    address: String,
    current: Mutex<String>,
    real: String,
    asked: Mutex<Asked>,
}

impl Source {
    fn new(address: String, first: &str, real: String) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("live"),
            address,
            current: Mutex::new(first.to_owned()),
            real,
            asked: Mutex::new(Asked::default()),
        })
    }

    fn asked(&self) -> Asked {
        *self.asked.lock().unwrap()
    }

    fn count(&self, request: &CredentialRequest<'_>, replace: bool) {
        let mut asked = self.asked.lock().unwrap();
        let slot = match request.protocol {
            CredentialProtocol::Imap => &mut asked.imap,
            _ => &mut asked.http,
        };
        if replace {
            slot.1 += 1;
        } else {
            slot.0 += 1;
        }
    }
}

#[async_trait]
impl CredentialSource for Source {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        self.count(request, false);
        let token = self.current.lock().unwrap().clone();
        Ok(Credential::bearer_for(self.address.clone(), token))
    }

    async fn replace(
        &self,
        _: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        self.count(request, true);
        self.real.clone_into(&mut self.current.lock().unwrap());
        Ok(Some(Credential::bearer_for(
            self.address.clone(),
            self.real.clone(),
        )))
    }
}

/// The account's address, which XOAUTH2 names beside the token.
async fn address(token: &str) -> String {
    let profile: serde_json::Value = engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("http client")
        .get("https://gmail.googleapis.com/gmail/v1/users/me/profile")
        .bearer_auth(token)
        .send()
        .await
        .expect("profile")
        .json()
        .await
        .expect("profile json");
    profile["emailAddress"]
        .as_str()
        .expect("address")
        .to_owned()
}

fn client(source: &Arc<Source>) -> GoogleClient {
    GoogleClient::connect_with_source(
        Arc::clone(source) as Arc<dyn CredentialSource>,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

/// The account's mail over IMAP, signed in through `source`: its first dial is the ask.
async fn imap(
    source: &Arc<Source>,
) -> Result<GmailMail<impl AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static>, String> {
    let config = ImapConfig::from_credential_source(
        "imap.gmail.com:993".to_owned(),
        "imap.gmail.com".to_owned(),
        Arc::clone(source) as Arc<dyn CredentialSource>,
    );
    let account = ImapAccount::connect(&config, engine_tls::TlsClientConfig::bundled().connector())
        .await
        .map_err(|err| format!("{:?}", ProviderError::from(err).class()))?;
    account.gmail().ok_or_else(|| "no X-GM-EXT-1".to_owned())
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

#[tokio::test]
async fn live_a_refused_token_is_replaced_and_the_labels_listed() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let source = Source::new(address(&token).await, GARBAGE, token);
    let provider = GmailProvider::new(client(&source));

    let labels = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("listed with the replacement");

    assert!(labels.is_snapshot());
    eprintln!("asked={:?}", source.asked());
    assert_eq!(
        source.asked().http.1,
        1,
        "one replacement, for the garbage token"
    );
}

#[tokio::test]
async fn live_the_control_a_garbage_token_without_a_replacement_is_refused() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let source = Source::new(address(&token).await, GARBAGE, GARBAGE.to_owned());
    let provider = GmailProvider::new(client(&source));

    let err = provider.sync_mailboxes(&account(), None).await.unwrap_err();

    eprintln!("class={:?} asked={:?}", err.class(), source.asked());
    assert_eq!(err.class(), FailureClass::Authentication);
    assert_eq!(source.asked().http, (1, 1), "asked once, replaced once");
}

/// The IMAP half refuses the garbage token and the source replaces it; the API half, asking the
/// same source, then presents the replacement and is refused nothing.
#[tokio::test]
async fn live_a_token_the_imap_half_replaced_is_the_one_the_api_half_presents() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let source = Source::new(address(&token).await, GARBAGE, token);

    let mail = imap(&source).await.expect("signed in with the replacement");
    let after_imap = source.asked();
    eprintln!("after IMAP: asked={after_imap:?}");
    assert_eq!(
        after_imap.imap.1, 1,
        "the IMAP half refused the garbage token"
    );

    let provider = GmailProvider::new(client(&source))
        .with_imap(mail)
        .expect("one source for both halves");
    provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("labels");
    let asked = source.asked();
    eprintln!("after API: asked={asked:?}");
    assert!(asked.http.0 >= 1, "the API half asked the same source");
    assert_eq!(
        asked.http.1, 0,
        "and presented the replacement, refused nothing"
    );
}

/// The other order: the API half's refusal replaces the token, and the IMAP half signs in with
/// the replacement at its first try.
#[tokio::test]
async fn live_a_token_the_api_half_replaced_is_the_one_the_imap_half_presents() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let source = Source::new(address(&token).await, GARBAGE, token);

    GmailProvider::new(client(&source))
        .sync_mailboxes(&account(), None)
        .await
        .expect("labels with the replacement");
    assert_eq!(source.asked().http.1, 1);

    imap(&source).await.expect("signed in");
    let asked = source.asked();
    eprintln!("asked={asked:?}");
    assert!(asked.imap.0 >= 1, "the IMAP half asked the same source");
    assert_eq!(
        asked.imap.1, 0,
        "and presented the replacement, refused nothing"
    );
}

/// A Gmail provider whose IMAP half asks another source is refused before anything is read.
#[tokio::test]
async fn live_an_imap_half_asking_another_source_is_refused() {
    let Some(token) = token() else {
        eprintln!("skipping: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let address = address(&token).await;
    let api = Source::new(address.clone(), &token, token.clone());
    let other = Arc::new(Source {
        subject: CredentialSubject::new("live-other"),
        ..Arc::into_inner(Source::new(address, &token, token.clone())).expect("one owner")
    });

    let mail = imap(&other).await.expect("signed in");
    let refused = GmailProvider::new(client(&api)).with_imap(mail);

    assert_eq!(
        refused.map(|_| ()).unwrap_err().class(),
        FailureClass::InvalidState
    );
}
