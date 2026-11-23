//! Gated live integration: a stored draft's keywords over IMAP, against the Stalwart harness.
//!
//! What only a real server can answer: that the Drafts folder's `SELECT` reports `\*` in
//! `PERMANENTFLAGS`, that an `APPEND` carrying the keyword beside `\Draft \Seen` stores it, so
//! the draft syncs back carrying it, and that a re-save asking for none stores a draft without
//! it.
//!
//! Skips with no `STALWART_IMAP_ADDR`, so `cargo test --workspace` stays green offline.

use std::{collections::BTreeSet, time::Duration as StdDuration};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Keyword, MailboxRole, Message, MessageMetadata},
    sync::SyncUpdate,
};
use engine_provider::{Credential, Draft, Provider};
use provider_imap::{ImapAccount, ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use tokio_rustls::{TlsConnector, client::TlsStream};

/// Accepts the harness's self-signed certificate. Test-only, never a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

fn account() -> AccountId {
    AccountId::try_from("imap-live-draft-keywords").unwrap()
}

fn keyword() -> Keyword {
    Keyword::new("engine-draft-probe").unwrap()
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
        Credential::password(harness.account.as_str(), harness.password.as_str()),
    );
    ImapAccount::connect(&config, no_verify_connector())
        .await
        .map(|account| account.provider(MailboxId::try_from(mailbox).unwrap()))
        .expect("connect IMAP")
}

/// The account's Drafts folder by its role.
async fn drafts_folder(harness: &Harness) -> String {
    let sync = connect(harness, "INBOX")
        .await
        .sync_mailboxes(&account(), None)
        .await
        .expect("list folders");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Drafts))
        .map_or_else(|| "Drafts".to_owned(), |mailbox| mailbox.name)
}

fn draft(message_id: &str, from: &str) -> Draft {
    Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(from),
        vec![EmailAddress::new("bob@test.local")],
        "Draft keyword probe",
        "Saved as a draft by the live test (no SMTP).",
    )
}

/// The one message in `folder` carrying `message_id`, as a sync reads it.
async fn synced(harness: &Harness, folder: &str, message_id: &str) -> Message {
    let sync = connect(harness, folder)
        .await
        .sync_email(&account(), None)
        .await
        .expect("sync drafts");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("a cursorless sync is a snapshot");
    };
    let mut carrying: Vec<Message> = objects
        .into_iter()
        .filter(|m| {
            m.envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        })
        .collect();
    assert_eq!(carrying.len(), 1, "one draft carrying {message_id}");
    carrying.remove(0)
}

#[tokio::test]
async fn live_imap_a_draft_keeps_its_keyword_and_a_re_save_without_it_drops_it() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_draft_keywords: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let folder = drafts_folder(&harness).await;
    let provider = connect(&harness, &folder).await;
    assert!(provider.connection_info().capabilities.draft_keywords());
    let message_id = &format!(
        "live-imap-draft-kw-{}@test.local",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos()
    );
    let tagged = BTreeSet::from([keyword()]);

    let first = provider
        .put_draft(
            &account(),
            &draft(message_id, &harness.account),
            None,
            &MessageMetadata::default().with_keywords(tagged.clone()),
        )
        .await
        .expect("first save");
    let stored = synced(&harness, &folder, message_id).await;

    let second = provider
        .put_draft(
            &account(),
            &draft(message_id, &harness.account),
            Some(&first.key),
            &MessageMetadata::default(),
        )
        .await
        .expect("second save");
    let resaved = synced(&harness, &folder, message_id).await;

    provider
        .delete_draft(&account(), &second.key)
        .await
        .expect("discard");

    // Stalwart's Drafts allows new keywords, so the receipt says the keyword was kept, and
    // the draft read back carries it. Both directions: the re-save without it reads back
    // without it, so the keyword is not something every draft carries.
    assert_eq!(first.kept.keywords, tagged);
    assert!(
        stored.keywords.contains(&keyword()),
        "{:?}",
        stored.keywords
    );
    assert_eq!(second.kept.keywords, BTreeSet::new());
    assert!(
        !resaved.keywords.contains(&keyword()),
        "{:?}",
        resaved.keywords
    );
}
