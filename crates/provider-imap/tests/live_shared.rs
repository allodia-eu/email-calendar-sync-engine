//! Gated live shared-mailbox checks that need the **Stalwart** harness specifically: its SMTP
//! submission, its refusal to create folders in someone else's store, and its second seeded
//! credential. What every server must do alike — discovery, the credential's own folder list,
//! a full share and a read-only one — is `live_imap_shared_contract.rs`, run against Dovecot
//! too.
//!
//! The host's flow is the one exercised: log in once, take the shared store's view of that
//! account with `ImapAccount::with_shared_mailbox`, list the store's folders through a provider
//! bound to a placeholder, then bind one per folder to an id *from that list* — no path is
//! built here that a host could not have been handed.
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
use provider_imap::{ImapAccount, ImapConfig, ImapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT};
use tokio_rustls::client::TlsStream;

type Live = ImapProvider<TlsStream<tokio::net::TcpStream>>;
type LiveAccount = ImapAccount<TlsStream<tokio::net::TcpStream>>;

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

async fn login(config: &ImapConfig) -> LiveAccount {
    ImapAccount::connect(
        config,
        engine_tls::TlsClientConfig::dangerous_accept_any().connector(),
    )
    .await
    .expect("connect IMAP")
}

fn bind(account: &LiveAccount, mailbox: &str) -> Live {
    account.provider(MailboxId::try_from(mailbox).unwrap())
}

async fn as_alice(harness: &Harness) -> LiveAccount {
    login(&config(harness, &harness.account, &harness.password)).await
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

/// Logs in with `config`, discovers `address`, then takes that store's view of the account —
/// the two halves of onboarding, over one login.
async fn scoped_to(config: &ImapConfig, address: &str) -> (SharedMailbox, LiveAccount) {
    let account = login(config).await;
    let store = bind(&account, "INBOX")
        .resolve_shared_mailbox(address)
        .await
        .unwrap_or_else(|err| panic!("{address} should resolve: {err}"));
    let shared = account.with_shared_mailbox(store.handle.clone());
    (store, shared)
}

fn alice(harness: &Harness) -> ImapConfig {
    config(harness, &harness.account, &harness.password)
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
    let bob = bind(
        &login(&config(&harness, &owner.address, &owner.password)).await,
        "INBOX",
    );
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
    let alice = as_alice(&harness).await;
    // Well-formed, under the shared namespace — and nothing there for alice.
    let gone = alice.with_shared_mailbox(
        SharedMailboxId::try_from("Shared Folders/nobody@test.local").unwrap(),
    );
    let err = bind(&gone, "INBOX")
        .sync_mailboxes(&account(), None)
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);

    // Not under any foreign namespace at all: not a handle discovery produced.
    let bogus = alice.with_shared_mailbox(SharedMailboxId::try_from("Archive").unwrap());
    let err = bind(&bogus, "INBOX")
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
    let (_, store) = scoped_to(&alice(&harness), &owner).await;
    let inbox = folders(&bind(&store, "INBOX")).await.remove(0);
    let shared = bind(&store, inbox.id.as_str());

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

    let own = as_alice(&harness).await;
    let own_drafts = folders(&bind(&own, "INBOX"))
        .await
        .into_iter()
        .find(|f| f.role == Some(MailboxRole::Drafts))
        .expect("alice has a Drafts");
    let drafts = mail(&bind(&own, own_drafts.id.as_str())).await;
    assert!(!has_message_id(&drafts, message_id), "{drafts:?}");
}

#[tokio::test]
async fn live_a_message_sent_as_the_group_is_filed_in_the_groups_sent() {
    let Some(harness) = ready("live_a_message_sent_as_the_group_is_filed_in_the_groups_sent")
    else {
        return;
    };
    let config = alice(&harness).with_smtp(harness.smtp_addr.as_str());
    let (_, store) = scoped_to(&config, SHARED_GROUP_ACCOUNT).await;
    let listing = folders(&bind(&store, "INBOX")).await;
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
    let shared = bind(&store, inbox.id.as_str());

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
    let sent = mail(&bind(&store, group_sent.id.as_str())).await;
    assert!(has_message_id(&sent, &message_id));
    let alice = as_alice(&harness).await;
    let own_sent = folders(&bind(&alice, "INBOX"))
        .await
        .into_iter()
        .find(|f| f.role == Some(MailboxRole::Sent))
        .expect("alice has a Sent");
    let own = mail(&bind(&alice, own_sent.id.as_str())).await;
    assert!(!has_message_id(&own, &message_id), "filed in alice's Sent");
}
