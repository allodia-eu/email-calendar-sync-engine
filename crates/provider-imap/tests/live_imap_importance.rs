//! Gated live integration: a message's importance survives a round trip through a real IMAP
//! server, against the Stalwart harness.
//!
//! A draft is stored at each importance (`APPEND` of the assembled bytes) and read back by a
//! metadata sync, so the assertion covers both halves: that the assembler wrote the headers and
//! that the `UID FETCH` header list asks for them in a shape the server answers. The offline mock
//! answers canned bytes whatever it is asked, so a field missing from the request passes there.
//! The normal draft is the control: it carries neither header, and reads back normal.
//!
//! Skips with no `STALWART_IMAP_ADDR`, so `cargo test --workspace` stays green offline.

use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Importance, MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider};
use provider_imap::{Credentials, ImapAccount, ImapConfig, ImapProvider};
use stalwart_harness::Harness;
use tokio_rustls::{TlsConnector, client::TlsStream};

/// Accepts the harness's self-signed certificate. Test-only, never a host trust store.
fn no_verify_connector() -> TlsConnector {
    engine_tls::TlsClientConfig::dangerous_accept_any().connector()
}

fn account() -> AccountId {
    AccountId::try_from("imap-live-importance").unwrap()
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
    ImapAccount::connect(&config, no_verify_connector())
        .await
        .map(|account| account.provider(MailboxId::try_from(mailbox).unwrap()))
        .expect("connect IMAP")
}

async fn drafts_folder(harness: &Harness) -> String {
    let provider = connect(harness, "INBOX").await;
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("list folders")
        .update
    else {
        return "Drafts".to_owned();
    };
    objects
        .into_iter()
        .find(|mailbox| mailbox.role == Some(MailboxRole::Drafts))
        .map_or_else(|| "Drafts".to_owned(), |mailbox| mailbox.name)
}

/// The stored message carrying `message_id`, as a metadata sync of `folder` reads it.
async fn stored(harness: &Harness, folder: &str, message_id: &MessageIdHeader) -> Message {
    let SyncUpdate::Snapshot { objects, .. } = connect(harness, folder)
        .await
        .sync_email(&account(), None)
        .await
        .expect("sync drafts")
        .update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
        .into_iter()
        .find(|message| message.envelope.message_id.contains(message_id))
        .expect("the saved draft is in the folder")
}

#[tokio::test]
async fn live_imap_a_drafts_importance_reads_back() {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping live_imap_a_drafts_importance_reads_back: STALWART_IMAP_ADDR unset");
        return;
    };
    harness
        .wait_until_ready(StdDuration::from_secs(30))
        .expect("harness ready");
    let folder = drafts_folder(&harness).await;
    let provider = connect(&harness, &folder).await;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();

    for importance in [Importance::High, Importance::Low, Importance::Normal] {
        let message_id = MessageIdHeader::new(format!(
            "live-imap-importance-{}-{nanos}@test.local",
            importance.as_str()
        ))
        .unwrap();
        let draft = Draft::new(
            message_id.clone(),
            EmailAddress::new(&harness.account),
            vec![EmailAddress::new("bob@test.local")],
            "Importance probe",
            "Saved as a draft by the live test (no SMTP).",
        )
        .with_importance(importance);
        let key = provider
            .put_draft(&account(), &draft, None, &std::collections::BTreeSet::new())
            .await
            .map(|stored| stored.key)
            .expect("save");

        let read = stored(&harness, &folder, &message_id).await;
        assert_eq!(read.envelope.importance, importance);

        provider
            .delete_draft(&account(), &key)
            .await
            .expect("discard");
    }
}
