//! Gated live integration: storing and replacing a draft over IMAP, against the
//! Stalwart harness.
//!
//! What only a real server can answer: that the `APPEND` carries flags the server
//! accepts, that `APPENDUID` comes back in the shape the key parser expects, and that
//! `UID EXPUNGE` actually removes the superseded copy. The offline mock answers canned
//! bytes whatever it is sent, so a wrong command shape passes there and fails here.
//!
//! Skips with no `STALWART_IMAP_ADDR`, so `cargo test --workspace` stays green offline.

use std::time::Duration as StdDuration;

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_imap::{Credentials, ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use tokio_rustls::{TlsConnector, client::TlsStream};

/// Accepts the harness's self-signed certificate. Test-only, never a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

async fn connect(
    harness: &Harness,
    mailbox: &str,
) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        Credentials::password(harness.account.as_str(), harness.password.as_str()),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

/// The account's folder carrying `role`, else `default_name`.
async fn role_folder(harness: &Harness, role: MailboxRole, default_name: &str) -> String {
    let provider = connect(harness, "INBOX").await;
    let account = AccountId::try_from("imap-drafts-resolve").unwrap();
    let sync = provider
        .sync_mailboxes(&account, None)
        .await
        .expect("list folders");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        return default_name.to_owned();
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role.as_ref() == Some(&role))
        .map_or_else(|| default_name.to_owned(), |mailbox| mailbox.name)
}

fn draft(message_id: &str, subject: &str, account: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(account),
        vec![EmailAddress::new("bob@test.local")],
        subject,
        "Saved as a draft by the live test (no SMTP).",
    )
}

/// How many messages the Drafts folder holds carrying `message_id`.
async fn copies_of(harness: &Harness, folder: &str, message_id: &str) -> usize {
    let provider = connect(harness, folder).await;
    let account = AccountId::try_from("imap-drafts-count").unwrap();
    let sync = provider
        .sync_email(&account, None)
        .await
        .expect("sync drafts");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .iter()
        .filter(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .count()
}

#[tokio::test]
async fn live_imap_replacing_a_draft_leaves_exactly_one() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_replacing_a_draft: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");

    let folder = role_folder(&harness, MailboxRole::Drafts, "Drafts").await;
    let provider = connect(&harness, &folder).await;
    let account = AccountId::try_from("imap-live-drafts").unwrap();
    // A fresh `Message-ID` per run: the count below is of copies carrying it, so a run
    // that aborted before its cleanup would otherwise be counted by every later run.
    let message_id = &format!(
        "live-imap-draft-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );

    // The server says it can keep a draft, which is what a host reads before offering
    // the action.
    assert!(provider.connection_info().capabilities.mail_drafts());

    let first = provider
        .put_draft(
            &account,
            &draft(message_id, "First version", &harness.account),
            None,
        )
        .await
        .expect("first save");
    assert_eq!(
        copies_of(&harness, &folder, message_id).await,
        1,
        "one save, one draft"
    );

    // Saving again supersedes it. The key moves, because an IMAP message is immutable.
    let second = provider
        .put_draft(
            &account,
            &draft(message_id, "Second version", &harness.account),
            Some(&first),
        )
        .await
        .expect("second save");
    assert_ne!(first, second, "a replaced IMAP draft is a new message");

    // The whole point: the folder holds the new version alone, so an edited draft does
    // not accumulate a copy per save.
    assert_eq!(
        copies_of(&harness, &folder, message_id).await,
        1,
        "replacing left a duplicate behind"
    );

    provider
        .delete_draft(&account, &second)
        .await
        .expect("discard");
    assert_eq!(copies_of(&harness, &folder, message_id).await, 0);

    // Retried, because the op behind it is retryable: the second call names a message
    // the first one removed, and has to settle rather than park.
    provider
        .delete_draft(&account, &second)
        .await
        .expect("a second discard is done, not an error");
}
