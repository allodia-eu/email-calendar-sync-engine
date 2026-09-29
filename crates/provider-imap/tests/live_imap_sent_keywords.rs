//! Gated live integration: a send asks for keywords on its filed Sent copy, and the copy
//! carries them.
//!
//! The offline suite asserts the `APPEND` flags; only a real server says whether it kept a
//! new keyword, since one that allows none answers `OK` and stores nothing. The copy is read
//! back from Sent, so a keyword the server dropped fails here. Every `Message-ID` is unique
//! to the run, and the copy is deleted afterwards. Skips with no `STALWART_IMAP_ADDR`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Keyword, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, Provider};
use provider_imap::{Credentials, ImapAccount, ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use tokio_rustls::client::TlsStream;

type Live = ImapProvider<TlsStream<tokio::net::TcpStream>>;

fn account() -> AccountId {
    AccountId::try_from("imap-live-sent-keywords").unwrap()
}

async fn connect(harness: &Harness, mailbox: &str) -> Live {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        Credentials::password(harness.account.as_str(), harness.password.as_str()),
    )
    .with_smtp(harness.smtp_addr.as_str());
    let connector = engine_tls::TlsClientConfig::dangerous_accept_any().connector();
    ImapAccount::connect(&config, connector)
        .await
        .map(|account| account.provider(MailboxId::try_from(mailbox).unwrap()))
        .expect("connect IMAP+SMTP")
}

/// The account's Sent folder by its SPECIAL-USE role ("Sent Items" on Stalwart).
async fn sent_folder(harness: &Harness) -> String {
    let SyncUpdate::Snapshot { objects, .. } = connect(harness, "INBOX")
        .await
        .sync_mailboxes(&account(), None)
        .await
        .expect("list folders")
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Sent))
        .map_or_else(|| "Sent".to_owned(), |mailbox| mailbox.name)
}

#[tokio::test]
async fn the_filed_copy_carries_the_keywords_the_send_asked_for() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping the_filed_copy_carries_the_keywords_...: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    let sent = connect(&harness, &sent_folder(&harness).await).await;
    assert!(sent.connection_info().capabilities.sent_copy_keywords());

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id =
        MessageIdHeader::new(format!("live-imap-sent-keywords-{nanos}@test.local")).unwrap();
    let keyword = Keyword::new("project-x").unwrap();
    let draft = Draft::new(
        message_id.clone(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new(harness.account.as_str())],
        "Sent copy keywords over IMAP",
        "The filed copy of this message carries a keyword.",
    )
    .with_sent_copy_keyword(keyword.clone());

    let receipt = sent.submit_email(&account(), &draft).await.expect("submit");
    assert!(receipt.sent_copy.is_filed());
    assert!(
        receipt.sent_copy_keywords.contains(&keyword),
        "the receipt says the copy kept the keyword: {:?}",
        receipt.sent_copy_keywords
    );

    let SyncUpdate::Snapshot { objects, .. } = sent
        .sync_email(&account(), None)
        .await
        .expect("sync Sent")
        .update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    let copy = objects
        .into_iter()
        .find(|message| message.envelope.message_id.contains(&message_id))
        .expect("the copy is filed in Sent");
    let carries = copy.keywords.contains(&keyword);
    sent.edit_mail(&account(), &MailEdit::delete(copy.id.key().clone()))
        .await
        .expect("cleanup delete");
    assert!(
        carries,
        "the filed copy carries the keyword: {:?}",
        copy.keywords
    );
}
