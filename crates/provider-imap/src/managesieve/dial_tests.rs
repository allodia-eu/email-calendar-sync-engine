//! What a ManageSieve dial asks the credential source, and how a refused credential is replaced.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use engine_provider::{Credential, CredentialRequest, CredentialSource, CredentialSubject};

use super::*;
use crate::mock::{MockStream, Recorded, written};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!("../../tests/fixtures/managesieve/", $name, ".txt")).as_slice()
    };
}

/// A password source that hands out `first`, and `then` when asked to replace it.
#[derive(Debug)]
struct Passwords {
    subject: CredentialSubject,
    first: &'static str,
    then: Option<&'static str>,
    asked: Mutex<Vec<(CredentialProtocol, Origin)>>,
}

#[async_trait]
impl CredentialSource for Passwords {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, engine_provider::ProviderError> {
        let asked = (request.protocol, request.origin.clone());
        self.asked.lock().unwrap().push(asked);
        Ok(Credential::password("alice@example.com", self.first))
    }

    async fn replace(
        &self,
        _refused: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, engine_provider::ProviderError> {
        let asked = (request.protocol, request.origin.clone());
        self.asked.lock().unwrap().push(asked);
        Ok(self
            .then
            .map(|password| Credential::password("alice@example.com", password)))
    }
}

fn source(first: &'static str, then: Option<&'static str>) -> Arc<Passwords> {
    Arc::new(Passwords {
        subject: CredentialSubject::new("alice"),
        first,
        then,
        asked: Mutex::default(),
    })
}

fn endpoint(addr: &str) -> SieveEndpoint {
    SieveEndpoint {
        addr: addr.to_owned(),
        server_name: "Sieve.Example.com".to_owned(),
    }
}

/// Servers answering one connection each, in order, with a TLS greeting and `answer`.
fn servers(answers: Vec<&'static [u8]>) -> (Arc<Mutex<Vec<Recorded>>>, impl FnMut() -> Opening) {
    let opened: Arc<Mutex<Vec<Recorded>>> = Arc::default();
    let mut answers = answers.into_iter();
    let record = Arc::clone(&opened);
    let open = move || {
        let mut script = fixture!("stalwart_greeting_tls").to_vec();
        script.extend_from_slice(answers.next().expect("no more connections"));
        let (stream, recorded) = MockStream::new(script);
        record.lock().unwrap().push(recorded);
        Box::pin(Session::open(stream)) as Opening
    };
    (opened, open)
}

type Opening = std::pin::Pin<Box<dyn Future<Output = SieveResult<Session<MockStream>>> + Send>>;

#[tokio::test]
async fn a_refused_password_is_replaced_once_on_a_fresh_connection() {
    let passwords = source("old", Some("new"));
    let config = ImapConfig::from_credential_source("h:993", "h", passwords.clone());
    let (opened, open) = servers(vec![
        fixture!("stalwart_authenticate_refused"),
        fixture!("stalwart_authenticate"),
    ]);
    dial_with(&config, &endpoint("127.0.0.1:14190"), open)
        .await
        .expect("the replacement signs in");

    // Each ask names ManageSieve and the `sieve` origin: the TLS name, lowercased, and the port.
    let origin = Origin::new("sieve", "sieve.example.com", 14190);
    assert_eq!(
        *passwords.asked.lock().unwrap(),
        [
            (CredentialProtocol::ManageSieve, origin.clone()),
            (CredentialProtocol::ManageSieve, origin),
        ]
    );
    let opened = opened.lock().unwrap();
    assert_eq!(
        opened.len(),
        2,
        "a second connection for the second credential"
    );
    assert!(written(&opened[1]).starts_with("AUTHENTICATE \"PLAIN\""));
}

#[tokio::test]
async fn a_refusal_with_no_replacement_fails_and_nothing_else_asks_again() {
    let passwords = source("old", None);
    let config = ImapConfig::from_credential_source("h:993", "h", passwords.clone());
    let (opened, open) = servers(vec![fixture!("stalwart_authenticate_refused")]);
    let err = dial_with(&config, &endpoint("sieve.example.com"), open)
        .await
        .unwrap_err();
    assert!(err.is_sign_in_refusal());
    assert_eq!(opened.lock().unwrap().len(), 1);
    // No port in the address: RFC 5804's.
    assert_eq!(
        passwords.asked.lock().unwrap()[0].1,
        Origin::new("sieve", "sieve.example.com", 4190)
    );

    // A failure that judged no credential asks for no replacement.
    let passwords = source("old", Some("new"));
    let config = ImapConfig::from_credential_source("h:993", "h", passwords.clone());
    let (_, open) = servers(vec![b"NO (TRYLATER) \"later\"\r\n"]);
    let err = dial_with(&config, &endpoint("h:4190"), open)
        .await
        .unwrap_err();
    assert!(!err.is_sign_in_refusal());
    assert_eq!(passwords.asked.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_server_name_that_is_no_tls_name_dials_nothing() {
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    let mut bad = endpoint("127.0.0.1:1");
    bad.server_name = "not a name".to_owned();
    // Refused before the connect, which to this address would fail as I/O.
    let err = open_tls(&bad, &connector).await.unwrap_err();
    assert!(
        matches!(err, SieveError::Imap(ImapError::Bad(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_oauthbearer_response_names_the_port_rfc_5804_assigns_where_the_address_names_none() {
    let config = ImapConfig::new(
        "h:993",
        "h",
        Credential::bearer_for("alice@example.com", "ya29.secret"),
    );
    let (opened, open) = servers(vec![b"OK\r\n"]);
    dial_with(&config, &endpoint("sieve.example.com"), open)
        .await
        .unwrap();
    let sent = written(&opened.lock().unwrap()[0]);
    let blob = sent
        .strip_prefix("AUTHENTICATE \"OAUTHBEARER\" \"")
        .and_then(|rest| rest.split('"').next())
        .unwrap();
    let decoded = String::from_utf8(crate::base64::decode(blob).unwrap()).unwrap();
    assert!(decoded.contains("\u{1}port=4190\u{1}"), "{decoded:?}");
}
