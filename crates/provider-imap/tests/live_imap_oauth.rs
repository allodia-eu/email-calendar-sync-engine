//! Live IMAP/SMTP **OAuth 2.0** tests against a real provider (issue #191).
//!
//! Gated on `IMAP_OAUTH_HOST` / `IMAP_OAUTH_USER` / `IMAP_OAUTH_TOKEN`; without them
//! every test skips, so the offline suite stays green with no credentials. Unlike the
//! rest of `tests/live_*`, these do **not** target the Stalwart fixture: the point is a
//! real provider's SASL implementation, over a **verifying** TLS connector (bundled
//! Mozilla roots), because what is being proved is that bytes we believe are right are
//! bytes Google and Yahoo actually accept.
//!
//! # Which mechanism these prove
//!
//! Both targets advertise **both** mechanisms (observed; Yahoo's documentation says
//! otherwise), so the target does not decide which runs — the client's preference does,
//! and that is `OAUTHBEARER`. These tests therefore exercise `OAUTHBEARER` end to end.
//! `XOAUTH2` is proven against the same server out-of-band, by sending the crate's exact
//! blob for it by hand, because no adapter path selects it where `OAUTHBEARER` is on
//! offer. See `docs/agent-guidance/imap-smtp.md` → "Authentication".
//!
//! ```sh
//! # Gmail — the default `tools/google-oauth` scope is already `https://mail.google.com/`.
//! export IMAP_OAUTH_HOST=imap.gmail.com IMAP_OAUTH_USER=you@gmail.example
//! export IMAP_OAUTH_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)"
//! export IMAP_OAUTH_SMTP_HOST=smtp.gmail.com   # optional: also submit a message
//!
//! # Yahoo — mint with `tools/yahoo-oauth`.
//! export IMAP_OAUTH_HOST=imap.mail.yahoo.com IMAP_OAUTH_USER=you@yahoo.example
//! export IMAP_OAUTH_TOKEN="$(cargo run -q --manifest-path tools/yahoo-oauth/Cargo.toml -- token)"
//!
//! cargo test -p provider-imap --test live_imap_oauth -- --nocapture
//! ```

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::EmailAddress,
    sync::{SyncUpdate, SyncWindow},
};
use engine_provider::{Draft, Provider};
use futures_util::StreamExt as _;
use provider_imap::{Credentials, ImapConfig, ImapProvider};
use tokio_rustls::TlsConnector;

/// The account under test, from the environment; `None` skips.
struct Target {
    host: String,
    port: u16,
    user: String,
    token: String,
    mailbox: String,
}

impl Target {
    fn from_env() -> Option<Self> {
        let (host, user, token) = (
            std::env::var("IMAP_OAUTH_HOST").ok()?,
            std::env::var("IMAP_OAUTH_USER").ok()?,
            std::env::var("IMAP_OAUTH_TOKEN").ok()?,
        );
        Some(Self {
            host,
            port: env_port("IMAP_OAUTH_PORT", 993),
            user,
            token,
            mailbox: std::env::var("IMAP_OAUTH_MAILBOX").unwrap_or_else(|_| "INBOX".to_owned()),
        })
    }

    /// The dial, authenticating with `token` — a parameter so the wrong-token test
    /// reuses the whole configuration and changes only the credential.
    fn config(&self, token: &str) -> ImapConfig {
        ImapConfig::new(
            format!("{}:{}", self.host, self.port),
            self.host.clone(),
            Credentials::oauth2(self.user.clone(), token),
        )
    }
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// A real, verifying connector — never the harness's trust-nothing one. A live OAuth
/// test that skipped certificate verification would be proving less than it claims.
fn connector() -> TlsConnector {
    engine_tls::TlsClientConfig::bundled().connector()
}

/// Skips with a printed reason when the account is not configured.
macro_rules! target {
    () => {
        match Target::from_env() {
            Some(target) => target,
            None => {
                eprintln!("skipping: set IMAP_OAUTH_HOST/USER/TOKEN to run the OAuth live tests");
                return;
            }
        }
    };
}

#[tokio::test]
async fn an_access_token_authenticates_and_the_session_works() {
    let target = target!();
    let provider = ImapProvider::connect(
        &target.config(&target.token),
        connector(),
        MailboxId::try_from(target.mailbox.as_str()).expect("mailbox"),
    )
    .await
    .expect("the access token must authenticate");

    // Authenticating is not the same as having a usable session: a `LIST` proves the
    // server really moved into the authenticated state rather than answering `OK` and
    // leaving the connection where it was.
    let account = AccountId::try_from("oauth-live").expect("account");
    let folders = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("folder list over the authenticated session");
    let SyncUpdate::Snapshot { objects, .. } = &folders.update else {
        panic!("a folder list is always a snapshot");
    };
    assert!(!objects.is_empty(), "an account has at least one folder");
    println!(
        "authenticated on {} and listed {} folders",
        target.host,
        objects.len()
    );

    // And a metadata fetch, which is the first thing a host actually does.
    let mut stream = provider.stream_email(&account, None, SyncWindow::full(), 5, 5);
    if let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("a page of mail over the authenticated session");
        println!("fetched a first page over the OAuth session: {chunk:?}");
    }
}

#[tokio::test]
async fn a_rejected_token_is_an_authentication_failure_rather_than_a_hang() {
    let target = target!();
    // The failure path is the one that only a real server can prove: both mechanisms
    // answer a bad token with a challenge and then *wait* for the client to acknowledge
    // it before sending the tagged `NO`. A client that does not acknowledge does not get
    // an error — it gets a stalled connection, which is why this test has a timeout
    // rather than only an assertion on the outcome.
    let corrupted = format!("{}-not-a-valid-token", target.token);
    let config = target.config(&corrupted);
    let attempt = ImapProvider::connect(
        &config,
        connector(),
        MailboxId::try_from(target.mailbox.as_str()).expect("mailbox"),
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), attempt)
        .await
        .expect("the server's rejection must arrive, not hang the dial");

    let err = outcome.expect_err("a corrupted token must not authenticate");
    assert_eq!(
        err.failure_class(),
        FailureClass::Authentication,
        "a rejected token must tell the host to refresh, not look like a broken server: {err}"
    );
    // The server's own reason travels with it — this is what a support report reads.
    println!("rejected as expected: {err}");
}

#[tokio::test]
async fn a_token_also_authenticates_smtp_submission() {
    let target = target!();
    let Ok(smtp_host) = std::env::var("IMAP_OAUTH_SMTP_HOST") else {
        eprintln!("skipping: set IMAP_OAUTH_SMTP_HOST to also exercise SMTP submission");
        return;
    };
    let smtp_port = env_port("IMAP_OAUTH_SMTP_PORT", 465);
    let config = target
        .config(&target.token)
        .with_smtp_tls(format!("{smtp_host}:{smtp_port}"), smtp_host.clone());
    let provider = ImapProvider::connect(
        &config,
        connector(),
        MailboxId::try_from(target.mailbox.as_str()).expect("mailbox"),
    )
    .await
    .expect("authenticate");

    // Addressed to the account itself, so a live run never mails a third party.
    let message_id =
        MessageIdHeader::new(format!("oauth-live-{}@{}", std::process::id(), target.host))
            .expect("message id");
    let draft = Draft::new(
        message_id,
        EmailAddress::new(target.user.clone()),
        vec![EmailAddress::new(target.user.clone())],
        "provider-imap OAuth submission check",
        "Sent by the gated live test for issue #191.",
    );
    let account = AccountId::try_from("oauth-live").expect("account");
    let receipt = provider
        .submit_email(&account, &draft)
        .await
        .expect("SMTP AUTH with the access token, then delivery");
    println!("submitted over an OAuth-authenticated SMTP session: {receipt:?}");
}
