//! Gated live shared-mailbox checks that need the **Stalwart** harness specifically: its SMTP
//! submission, its refusal to create folders in someone else's store, and its second seeded
//! credential. What every server must do alike — discovery, the credential's own folder list,
//! a full share and a read-only one — is `live_imap_shared_contract.rs`, run against Dovecot
//! too.
//!
//! The host's flow is the one exercised: scope a provider with
//! `ImapConfig::with_shared_mailbox`, list the store's folders through a provider bound to a
//! placeholder, then bind one per folder to an id *from that list* — no path is built here
//! that a host could not have been handed.
//!
//! Every assertion is on content the harness controls — folder roles, rights letters, a
//! `Message-ID` — never on a server-assigned UID. Skips with no `STALWART_IMAP_ADDR`.

use std::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader, SharedMailboxId},
    mail::{EmailAddress, Mailbox, MailboxRole, Message},
    sync::{SyncObject, SyncUpdate},
};
use engine_provider::{Draft, Provider, SharedMailbox, SharedMailboxes};
use provider_imap::{ImapConfig, ImapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT};
use tokio_rustls::client::TlsStream;

type Live = ImapProvider<TlsStream<tokio::net::TcpStream>>;

fn account() -> AccountId {
    AccountId::try_from("live-shared").unwrap()
}

fn ready(test: &str) -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_IMAP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

fn host_of(addr: &str) -> &str {
    addr.rsplit_once(':').map_or("localhost", |(host, _)| host)
}

fn config(harness: &Harness, user: &str, password: &str) -> ImapConfig {
    ImapConfig::new(
        harness.imap_addr.as_str(),
        host_of(&harness.imap_addr),
        user,
        password,
    )
}

async fn connect(config: &ImapConfig, mailbox: &str) -> Live {
    ImapProvider::connect(
        config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

async fn as_alice(harness: &Harness, mailbox: &str) -> Live {
    connect(
        &config(harness, &harness.account, &harness.password),
        mailbox,
    )
    .await
}

fn snapshot<T: SyncObject>(update: SyncUpdate<T>) -> Vec<T> {
    let SyncUpdate::Snapshot { objects, .. } = update else {
        panic!("a first sync is a snapshot");
    };
    objects
}

async fn folders(provider: &Live) -> Vec<Mailbox> {
    snapshot(
        provider
            .sync_mailboxes(&account(), None)
            .await
            .expect("folder sync")
            .update,
    )
}

async fn mail(provider: &Live) -> Vec<Message> {
    snapshot(
        provider
            .sync_email(&account(), None)
            .await
            .expect("email sync")
            .update,
    )
}

fn has_message_id(messages: &[Message], id: &str) -> bool {
    messages
        .iter()
        .any(|m| m.envelope.message_id.iter().any(|i| i.as_str() == id))
}

/// Discovers `address` as alice, then scopes a config to it — the two halves of
/// onboarding.
async fn scoped_to(harness: &Harness, address: &str) -> (SharedMailbox, ImapConfig) {
    let store = as_alice(harness, "INBOX")
        .await
        .resolve_shared_mailbox(address)
        .await
        .unwrap_or_else(|err| panic!("{address} should resolve: {err}"));
    let config = config(harness, &harness.account, &harness.password)
        .with_shared_mailbox(store.handle.clone());
    (store, config)
}

#[tokio::test]
async fn live_a_credential_nothing_is_shared_with_gets_an_empty_list_not_a_refusal() {
    let Some(harness) =
        ready("live_a_credential_nothing_is_shared_with_gets_an_empty_list_not_a_refusal")
    else {
        return;
    };
    // Bob granted access rather than receiving it: the same server, and a credential with
    // nothing shared *with* it. The mechanism is there (`NAMESPACE` and `ACL`), so the
    // capability says so, and the honest answer is an empty list — "none yet", which a host
    // can show — rather than a refusal it would have to explain.
    let owner = harness.read_only_share_owner().clone();
    let bob = connect(&config(&harness, &owner.address, &owner.password), "INBOX").await;
    assert_eq!(
        bob.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );
    assert_eq!(bob.list_shared_mailboxes().await.unwrap(), []);
}

#[tokio::test]
async fn live_a_handle_the_credential_cannot_open_is_refused() {
    let Some(harness) = ready("live_a_handle_the_credential_cannot_open_is_refused") else {
        return;
    };
    // Well-formed, under the shared namespace — and nothing there for alice.
    let gone = config(&harness, &harness.account, &harness.password).with_shared_mailbox(
        SharedMailboxId::try_from("Shared Folders/nobody@test.local").unwrap(),
    );
    let err = connect(&gone, "INBOX")
        .await
        .sync_mailboxes(&account(), None)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);

    // Not under any foreign namespace at all: not a handle discovery produced.
    let bogus = config(&harness, &harness.account, &harness.password)
        .with_shared_mailbox(SharedMailboxId::try_from("Archive").unwrap());
    let err = connect(&bogus, "INBOX")
        .await
        .sync_mailboxes(&account(), None)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn live_a_draft_for_a_store_without_drafts_never_lands_in_the_credentials_own() {
    let Some(harness) =
        ready("live_a_draft_for_a_store_without_drafts_never_lands_in_the_credentials_own")
    else {
        return;
    };
    // Bob's share exposes one folder and so no `\Drafts`: saving there takes the fallback of
    // creating the conventional folder. Unqualified, that resolves in alice's own namespace,
    // where she already has a Drafts — the save would "succeed" with bob's draft in her
    // folder. Qualified, it targets bob's store, which Stalwart refuses outright
    // (`NO [CANNOT] … create root folders under shared folders`). The failure is the proof.
    let owner = harness.read_only_share_owner().address.clone();
    let (_, config) = scoped_to(&harness, &owner).await;
    let shared = connect(&config, "INBOX").await;
    let inbox = folders(&shared).await.remove(0);
    let shared = connect(&config, inbox.id.as_str()).await;

    let message_id = "imap-shared-draft-probe@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("nobody@test.local")],
        "Shared-store draft probe",
        "Never filed anywhere.",
    );
    let err = shared
        .save_draft(&draft)
        .await
        .expect_err("the fallback targets the share, which refuses the CREATE");
    assert!(!err.is_retryable(), "{err}");

    let own_drafts = folders(&as_alice(&harness, "INBOX").await)
        .await
        .into_iter()
        .find(|f| f.role == Some(MailboxRole::Drafts))
        .expect("alice has a Drafts");
    let drafts = mail(&as_alice(&harness, own_drafts.id.as_str()).await).await;
    assert!(!has_message_id(&drafts, message_id), "{drafts:?}");
}

#[tokio::test]
async fn live_a_message_sent_as_the_group_is_filed_in_the_groups_sent() {
    let Some(harness) = ready("live_a_message_sent_as_the_group_is_filed_in_the_groups_sent")
    else {
        return;
    };
    let (_, config) = scoped_to(&harness, SHARED_GROUP_ACCOUNT).await;
    let config = config.with_smtp(harness.smtp_addr.as_str());
    let listing = folders(&connect(&config, "INBOX").await).await;
    let group_sent = listing
        .iter()
        .find(|f| f.role == Some(MailboxRole::Sent))
        .expect("the group has a Sent folder")
        .clone();
    let inbox = listing
        .iter()
        .find(|f| f.role == Some(MailboxRole::Inbox))
        .unwrap()
        .clone();
    let shared = connect(&config, inbox.id.as_str()).await;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = format!("imap-shared-send-{nonce}@test.local");
    let draft = Draft::new(
        MessageIdHeader::new(&message_id).unwrap(),
        EmailAddress::named("Support", SHARED_GROUP_ACCOUNT),
        vec![EmailAddress::new(&harness.read_only_share_owner().address)],
        "Sent as the group mailbox over SMTP",
        "Sent by the shared-mailbox live suite.",
    );
    let receipt = shared.submit_email(&account(), &draft).await.expect("sent");
    // Filed into the group's Sent, which a flat first-match lookup could have missed.
    assert!(
        receipt.email_key.as_str().ends_with(group_sent.id.as_str()),
        "filed as {} — not in {}",
        receipt.email_key.as_str(),
        group_sent.id.as_str()
    );
    let sent = mail(&connect(&config, group_sent.id.as_str()).await).await;
    assert!(has_message_id(&sent, &message_id));
    let own_sent = folders(&as_alice(&harness, "INBOX").await)
        .await
        .into_iter()
        .find(|f| f.role == Some(MailboxRole::Sent))
        .expect("alice has a Sent");
    let own = mail(&as_alice(&harness, own_sent.id.as_str()).await).await;
    assert!(!has_message_id(&own, &message_id), "filed in alice's Sent");
}
