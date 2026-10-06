//! Live IMAP tests against a real provider account that signs in with a password (an app
//! password, on the providers that require one).
//!
//! Gated on `IMAP_LIVE_HOST` / `IMAP_LIVE_USER` / `IMAP_LIVE_PASSWORD` (and `IMAP_LIVE_PORT`,
//! default 993); without them every test skips. The harnesses all spell the inbox `INBOX` and
//! all offer QRESYNC, so the behaviour a real provider differs on is proven here: Yahoo, for
//! one, lists `Inbox` and enables no QRESYNC. Each test opens one connection, because a
//! provider that limits sign-ins counts every one.

use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, ProviderKey},
    mail::{Keyword, MailboxRole, SystemKeyword},
    sync::{SyncState, SyncUpdate, SyncWindow},
};
use engine_provider::{EmailChunk, MailEdit, Provider};
use futures_util::StreamExt as _;
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

async fn inbox(target: &Target) -> impl Provider {
    ImapAccount::connect(
        &target.config(),
        engine_tls::TlsClientConfig::bundled().connector(),
    )
    .await
    .expect("the password must authenticate")
    .provider("INBOX".try_into().unwrap())
}

fn account() -> AccountId {
    AccountId::try_from("live-password").unwrap()
}

/// One whole email pass over the inbox.
async fn pass(provider: &impl Provider, cursor: Option<&SyncState>) -> Vec<EmailChunk> {
    let account = account();
    let mut stream = provider.stream_email(&account, cursor, SyncWindow::full(), 200, 0);
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk.expect("the pass must complete"));
    }
    chunks
}

fn present(chunks: &[EmailChunk]) -> BTreeSet<ProviderKey> {
    chunks
        .iter()
        .flat_map(|c| c.present.iter().cloned())
        .collect()
}

fn final_cursor(chunks: &[EmailChunk]) -> SyncState {
    chunks.last().unwrap().advance_to.clone().unwrap()
}

/// Whether the pass's state change for `key` says it is flagged; `None` if it carried none.
fn flagged_in(chunks: &[EmailChunk], key: &ProviderKey) -> Option<bool> {
    let flagged = Keyword::system(SystemKeyword::Flagged);
    chunks
        .iter()
        .flat_map(|c| c.patched.iter())
        .find(|change| &change.key == key)
        .map(|change| change.state.keywords.contains(&flagged))
}

#[tokio::test]
async fn the_inbox_is_listed_once_and_as_inbox() {
    let target = target!();
    let provider = inbox(&target).await;
    let account = account();
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

#[tokio::test]
async fn a_flag_set_after_the_first_pass_comes_back_on_the_next() {
    // Whatever the session negotiated: on a server without QRESYNC this is the reconciling
    // delta, which reads every held message's flags; with QRESYNC, `CHANGEDSINCE`.
    let target = target!();
    let provider = inbox(&target).await;
    let account = account();
    let first = pass(&provider, None).await;
    let held = present(&first);
    let newest = first
        .iter()
        .flat_map(|c| c.changed.iter())
        .max_by_key(|m| m.id.key().as_str().to_owned())
        .expect("the inbox holds mail")
        .clone();
    let key = newest.id.key().clone();
    let was_flagged = newest
        .keywords
        .contains(&Keyword::system(SystemKeyword::Flagged));

    provider
        .edit_mail(&account, &MailEdit::set_flagged(key.clone(), !was_flagged))
        .await
        .unwrap();
    let second = pass(&provider, Some(&final_cursor(&first))).await;
    // Restore before asserting, so a failure leaves the account as it was found.
    provider
        .edit_mail(&account, &MailEdit::set_flagged(key.clone(), was_flagged))
        .await
        .unwrap();
    let third = pass(&provider, Some(&final_cursor(&second))).await;

    assert_eq!(
        flagged_in(&second, &key),
        Some(!was_flagged),
        "the change came back"
    );
    assert_eq!(
        flagged_in(&third, &key),
        Some(was_flagged),
        "and so did its undo"
    );
    if second.iter().any(EmailChunk::is_reconcile_final) {
        // The reconciling form: its present set is the whole inbox, as the snapshot's was.
        assert_eq!(present(&second), held);
        assert!(second.last().unwrap().patched.is_empty());
    }
}
