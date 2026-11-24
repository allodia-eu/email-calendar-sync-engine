//! Gated live proof that Microsoft Graph asks the account's credential source per request: a
//! token Graph refuses is replaced once and the request succeeds with the replacement, and a
//! `401` Graph sends without a challenge to the directory read asks for none (MICROSOFT-2 in
//! `docs/spec-deviations.md`).
//!
//! Read-only: it lists the mail folders and, on a personal account, asks for the directory
//! source. The first token presented is a garbage one Graph cannot have issued; the replacement
//! is `GRAPH_ACCESS_TOKEN`. Nothing is printed but counts and classes.
//!
//! ```sh
//! GRAPH_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/graph-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-graph --test live_credential_source -- --nocapture
//! ```

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_core::ids::{AccountId, MailboxId};
use engine_provider::{
    ContactSourceSync, ContactsProvider, Credential, CredentialRequest, CredentialSource,
    CredentialSubject, Provider, ProviderError, ProviderResult,
};
use provider_graph::{GraphClient, GraphContactProvider, GraphProvider};

fn token() -> Option<String> {
    std::env::var("GRAPH_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// A token Graph cannot have issued, shaped as one (three base64url parts).
const GARBAGE: &str = "eyJ0eXAiOiJKV1QifQ.eyJhdWQiOiJleGFtcGxlIn0.c2lnbmF0dXJl";

/// Answers `first` until asked to replace it, and `real` from then on, as a refreshing source
/// does; counts both.
#[derive(Debug)]
struct Source {
    subject: CredentialSubject,
    current: Mutex<String>,
    real: String,
    asks: Mutex<usize>,
    replaced: Mutex<usize>,
}

impl Source {
    fn new(first: &str, real: String) -> Arc<Self> {
        Arc::new(Self {
            subject: CredentialSubject::new("live"),
            current: Mutex::new(first.to_owned()),
            real,
            asks: Mutex::new(0),
            replaced: Mutex::new(0),
        })
    }

    fn counts(&self) -> (usize, usize) {
        (*self.asks.lock().unwrap(), *self.replaced.lock().unwrap())
    }
}

#[async_trait]
impl CredentialSource for Source {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(&self, _: &CredentialRequest<'_>) -> Result<Credential, ProviderError> {
        *self.asks.lock().unwrap() += 1;
        Ok(Credential::bearer(self.current.lock().unwrap().clone()))
    }

    async fn replace(
        &self,
        _: &Credential,
        _: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        *self.replaced.lock().unwrap() += 1;
        self.real.clone_into(&mut self.current.lock().unwrap());
        Ok(Some(Credential::bearer(self.real.clone())))
    }
}

fn client(source: &Arc<Source>) -> GraphClient {
    GraphClient::connect_with_source(
        Arc::clone(source) as Arc<dyn CredentialSource>,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
        &engine_http::test_server::user_agent(),
    )
    .expect("client")
}

/// `result`'s value. A failure panics with its class and `what` alone: a server's error text,
/// which `ProviderError`'s `Debug` carries, can echo the account's content.
fn ok<T>(result: ProviderResult<T>, what: &str) -> T {
    result.unwrap_or_else(|err| panic!("{what}: failed as {:?}", err.class()))
}

/// `result`'s failure. A success panics with `what` alone, never the value, which is mail.
fn failed<T>(result: ProviderResult<T>, what: &str) -> ProviderError {
    match result {
        Ok(_) => panic!("{what}: succeeded"),
        Err(err) => err,
    }
}

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

#[tokio::test]
async fn live_a_refused_token_is_replaced_and_the_folders_listed() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let source = Source::new(GARBAGE, token);
    let provider = GraphProvider::new(client(&source), MailboxId::try_from("inbox").unwrap());

    let folders = ok(
        provider.sync_mailboxes(&account(), None).await,
        "listed with the replacement",
    );

    assert!(folders.is_snapshot());
    // The listing's first request was refused and replaced; every later one asked again and
    // was given the replacement.
    let (asks, replaced) = source.counts();
    eprintln!("asks={asks} replaced={replaced}");
    assert_eq!(replaced, 1, "one replacement, for the garbage token");
}

#[tokio::test]
async fn live_the_control_a_garbage_token_without_a_replacement_is_refused() {
    let Some(_) = token() else {
        eprintln!("skipping: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    // The same garbage, and the source's replacement is the garbage again: what a source
    // with nothing better gets back, so the replacement above is what made it work.
    let source = Source::new(GARBAGE, GARBAGE.to_owned());
    let provider = GraphProvider::new(client(&source), MailboxId::try_from("inbox").unwrap());

    let err = failed(
        provider.sync_mailboxes(&account(), None).await,
        "a garbage token",
    );

    eprintln!("class={:?} counts={:?}", err.class(), source.counts());
    assert_eq!(
        err.class(),
        engine_core::error::FailureClass::Authentication
    );
    assert_eq!(source.counts(), (1, 1), "asked once, replaced once");
}

/// MICROSOFT-2, both directions. A personal account has no directory, and Graph says so with
/// a `401` that carries no challenge: with a good token, no replacement is asked for. With a
/// garbage token the same request is refused with a `Bearer` challenge, and one is.
#[tokio::test]
async fn live_a_401_without_a_challenge_asks_for_no_replacement() {
    let Some(token) = token() else {
        eprintln!("skipping: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let good = Source::new(&token, token.clone());
    let directory = GraphContactProvider::directory(client(&good));
    match directory.sync_contacts(&account(), None).await {
        Ok(ContactSourceSync::Unavailable(_)) => {}
        Ok(ContactSourceSync::Available { .. }) => {
            eprintln!(
                "!! NOT VERIFIED: this account has a directory (a work account); MICROSOFT-2 \
                 needs a personal one"
            );
            return;
        }
        Err(err) => panic!("the directory read failed: {:?}", err.class()),
    }
    eprintln!("good token: counts={:?}", good.counts());
    assert_eq!(
        good.counts().1,
        0,
        "no replacement for a 401 without a challenge"
    );

    let garbage = Source::new(GARBAGE, token);
    let directory = GraphContactProvider::directory(client(&garbage));
    let _ = directory.sync_contacts(&account(), None).await;
    eprintln!("garbage token: counts={:?}", garbage.counts());
    assert_eq!(
        garbage.counts().1,
        1,
        "the control: a refused token on the same request is replaced"
    );
}
