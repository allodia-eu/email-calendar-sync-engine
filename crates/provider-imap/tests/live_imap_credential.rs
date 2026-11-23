//! Gated live proof of the credential source on real servers: a source that presents a wrong
//! password first and the right one through `replace` signs in, which only works if the client
//! re-dials and presents the replacement after the server refused the first one.
//!
//! The server's refusal is what the assertions rest on: a source asked for no replacement would
//! mean the wrong password was accepted, and the test fails. Each ask is checked to name the
//! protocol and the server's TLS name.
//!
//! - **IMAP** on every configured server (Dovecot rev1 and rev2, Stalwart; `*_IMAP_ADDR`).
//! - **SMTP submission** on Stalwart, over implicit TLS (465) and `STARTTLS` (587)
//!   (`STALWART_HTTP_ADDR`). IMAP signs in with the right password there; only the submission's
//!   `AUTH` is refused first.
//!
//! ⚠️ Stalwart bans an IP after 100 failed sign-ins a day (`x:Security`'s `authBanRate`, v1.0.2),
//! and every harness test runs from 127.0.0.1, so a ban stops every live suite. One run of this
//! file spends three of them on Stalwart (one IMAP, one per submission port) and none on
//! Dovecot, which bans nobody. Repeated runs belong on Dovecot.

#[path = "common/imap_live.rs"]
mod imap_live;

use std::{sync::Mutex, time::Duration};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::EmailAddress,
};
use engine_provider::{
    Credential, CredentialProtocol, CredentialRequest, CredentialSource, CredentialSubject, Draft,
    HandOver, Origin, Provider, ProviderError, Unrecorded,
};
use imap_live::{DOVECOT_REV1, DOVECOT_REV2, STALWART, Server};
use provider_imap::{ImapAccount, ImapConfig};
use stalwart_harness::Harness;

/// A password source that answers wrong for `refused_over` and right through `replace`, and
/// records every ask.
#[derive(Debug)]
struct WrongThenRight {
    subject: CredentialSubject,
    username: String,
    password: String,
    refused_over: CredentialProtocol,
    asks: Mutex<Vec<(&'static str, CredentialProtocol, Origin)>>,
}

impl WrongThenRight {
    fn new(username: &str, password: &str, refused_over: CredentialProtocol) -> Self {
        Self {
            subject: CredentialSubject::new(username),
            username: username.to_owned(),
            password: password.to_owned(),
            refused_over,
            asks: Mutex::default(),
        }
    }

    fn right(&self) -> Credential {
        Credential::password(self.username.as_str(), self.password.as_str())
    }

    fn record(&self, ask: &'static str, request: &CredentialRequest<'_>) {
        let asked = (ask, request.protocol, request.origin.clone());
        self.asks.lock().unwrap().push(asked);
    }

    /// The asks made over `protocol`, by kind.
    fn asks_over(&self, protocol: CredentialProtocol) -> Vec<(&'static str, Origin)> {
        let asks = self.asks.lock().unwrap();
        let over = asks.iter().filter(|(_, asked, _)| *asked == protocol);
        over.map(|(ask, _, origin)| (*ask, origin.clone()))
            .collect()
    }
}

#[async_trait]
impl CredentialSource for WrongThenRight {
    fn subject(&self) -> &CredentialSubject {
        &self.subject
    }

    async fn credential(
        &self,
        request: &CredentialRequest<'_>,
    ) -> Result<Credential, ProviderError> {
        self.record("credential", request);
        if request.protocol == self.refused_over {
            let wrong = format!("{}-not-the-password", self.password);
            return Ok(Credential::password(self.username.as_str(), wrong));
        }
        Ok(self.right())
    }

    async fn replace(
        &self,
        _refused: &Credential,
        request: &CredentialRequest<'_>,
    ) -> Result<Option<Credential>, ProviderError> {
        self.record("replace", request);
        Ok(Some(self.right()))
    }
}

/// The host portion of a `host:port` address: the TLS name the harness is dialled as.
fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or("localhost", |(host, _)| host)
}

fn port_of(addr: &str) -> u16 {
    let port = addr.rsplit_once(':').map(|(_, port)| port.parse());
    port.expect("host:port").expect("a port number")
}

async fn imap_signs_in_with_the_replacement(server: &Server) {
    let Ok(addr) = std::env::var(server.addr_var) else {
        eprintln!("skipping on {}: {} unset", server.label, server.addr_var);
        return;
    };
    let source = std::sync::Arc::new(WrongThenRight::new(
        server.account,
        server.password,
        CredentialProtocol::Imap,
    ));
    let config = ImapConfig::from_credential_source(addr.as_str(), host_of(&addr), source.clone());

    let account = ImapAccount::connect(
        &config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
    )
    .await
    .unwrap_or_else(|err| panic!("{}: the replacement must sign in: {err}", server.label));
    // The session is real: it lists the account's folders.
    let provider = account.provider(MailboxId::try_from("INBOX").unwrap());
    let account_id = AccountId::try_from("live-credential").unwrap();
    provider
        .sync_mailboxes(&account_id, None)
        .await
        .unwrap_or_else(|err| panic!("{}: list folders: {err}", server.label));

    let origin = Origin::new("imaps", host_of(&addr), port_of(&addr));
    assert_eq!(
        source.asks_over(CredentialProtocol::Imap),
        [("credential", origin.clone()), ("replace", origin)],
        "{}: the wrong password was refused, and replaced once",
        server.label
    );
}

#[tokio::test]
async fn live_imap_a_refused_password_is_replaced_on_dovecot_rev1() {
    imap_signs_in_with_the_replacement(&DOVECOT_REV1).await;
}

#[tokio::test]
async fn live_imap_a_refused_password_is_replaced_on_dovecot_rev2() {
    imap_signs_in_with_the_replacement(&DOVECOT_REV2).await;
}

#[tokio::test]
async fn live_imap_a_refused_password_is_replaced_on_stalwart() {
    imap_signs_in_with_the_replacement(&STALWART).await;
}

/// How a submission is secured, with the scheme the source is told.
#[derive(Clone, Copy)]
enum Submission {
    ImplicitTls,
    StartTls,
}

async fn smtp_sends_with_the_replacement(submission: Submission) {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping the live SMTP credential test: STALWART_HTTP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let source = std::sync::Arc::new(WrongThenRight::new(
        &harness.account,
        &harness.password,
        CredentialProtocol::Smtp,
    ));
    let imap = harness.imap_addr.as_str();
    let config = ImapConfig::from_credential_source(imap, host_of(imap), source.clone());
    let (config, smtp, scheme, marker) = match submission {
        Submission::ImplicitTls => {
            let smtp = harness.smtp_tls_addr.as_str();
            let config = config.with_smtp_tls(smtp, host_of(smtp));
            (config, smtp, "submissions", "cred-replace-implicit")
        }
        Submission::StartTls => {
            let smtp = harness.smtp_starttls_addr.as_str();
            let config = config.with_smtp_starttls(smtp, host_of(smtp));
            (config, smtp, "submission", "cred-replace-starttls")
        }
    };
    let provider = ImapAccount::connect(
        &config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
    )
    .await
    .expect("IMAP signs in with the right password")
    .provider(MailboxId::try_from("INBOX").unwrap());

    let message_id = format!("{marker}@test.local");
    let draft = Draft::new(
        MessageIdHeader::new(&message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("bob@test.local")],
        marker,
        "Sent after the submission server refused the first password.",
    );
    let account_id = AccountId::try_from("live-credential").unwrap();
    let receipt = provider
        .submit_email(&account_id, &draft, &HandOver::new(&Unrecorded))
        .await
        .expect("the replacement password submits");
    assert_eq!(receipt.message_id.as_str(), message_id);

    let origin = Origin::new(scheme, host_of(smtp), port_of(smtp));
    assert_eq!(
        source.asks_over(CredentialProtocol::Smtp),
        [("credential", origin.clone()), ("replace", origin)],
        "the wrong password was refused at AUTH, and replaced once"
    );
}

#[tokio::test]
async fn live_smtp_a_refused_password_is_replaced_over_implicit_tls() {
    smtp_sends_with_the_replacement(Submission::ImplicitTls).await;
}

#[tokio::test]
async fn live_smtp_a_refused_password_is_replaced_over_starttls() {
    smtp_sends_with_the_replacement(Submission::StartTls).await;
}
