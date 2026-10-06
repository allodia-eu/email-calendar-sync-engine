//! Live IMAP tests against a real provider account that signs in with a password (an app
//! password, on the providers that require one).
//!
//! Gated on `IMAP_LIVE_HOST` / `IMAP_LIVE_USER` / `IMAP_LIVE_PASSWORD` (and `IMAP_LIVE_PORT`,
//! default 993); without them every test skips. The harnesses all spell the inbox `INBOX` and
//! all offer QRESYNC, so the behaviour a real provider differs on is proven here: Yahoo, for
//! one, lists `Inbox` and enables no QRESYNC. Each test opens one connection, because a
//! provider that limits sign-ins counts every one.

use engine_core::{ids::AccountId, mail::MailboxRole, sync::SyncUpdate};
use engine_provider::Provider;
use provider_imap::{Credentials, ImapAccount, ImapConfig};

/// The account under test, from the environment; `None` skips.
struct Target {
    host: String,
    port: u16,
    user: String,
    password: String,
}

impl Target {
    fn from_env() -> Option<Self> {
        Some(Self {
            host: std::env::var("IMAP_LIVE_HOST").ok()?,
            port: std::env::var("IMAP_LIVE_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(993),
            user: std::env::var("IMAP_LIVE_USER").ok()?,
            password: std::env::var("IMAP_LIVE_PASSWORD").ok()?,
        })
    }

    fn config(&self) -> ImapConfig {
        ImapConfig::new(
            format!("{}:{}", self.host, self.port),
            self.host.clone(),
            Credentials::password(self.user.clone(), self.password.clone()),
        )
    }
}

/// Skips with a printed reason when the account is not configured.
macro_rules! target {
    () => {
        match Target::from_env() {
            Some(target) => target,
            None => {
                eprintln!("skipping: set IMAP_LIVE_HOST/USER/PASSWORD to run these tests");
                return;
            }
        }
    };
}

#[tokio::test]
async fn the_inbox_is_listed_once_and_as_inbox() {
    let target = target!();
    let provider = ImapAccount::connect(
        &target.config(),
        engine_tls::TlsClientConfig::bundled().connector(),
    )
    .await
    .expect("the password must authenticate")
    .provider("INBOX".try_into().unwrap());
    let account = AccountId::try_from("live-password").unwrap();
    let listing = provider.sync_mailboxes(&account, None).await.unwrap();
    let SyncUpdate::Snapshot { objects, .. } = listing.update else {
        panic!("a folder list is a snapshot");
    };
    let inboxes: Vec<_> = objects
        .iter()
        .filter(|mailbox| mailbox.role == Some(MailboxRole::Inbox))
        .collect();
    assert_eq!(inboxes.len(), 1, "{objects:?}");
    // The id a host binds the inbox by, whatever the server called it.
    assert_eq!(inboxes[0].id.as_str(), "INBOX");
}
