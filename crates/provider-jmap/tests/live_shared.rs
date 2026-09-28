//! Gated live shared-mailbox checks against the Stalwart harness.
//!
//! The offline suite replays a captured session, so it proves the *reading*. What it cannot
//! prove is that a real server lists the shares, accepts calls carrying a shared
//! `accountId`, hands back that store's mail and rights, and lets the credential send as a
//! store it belongs to. Those are here.
//!
//! Every account id is resolved **by name**: Stalwart assigns them in creation order, so a
//! test that hard-coded one would be asserting on a server-assigned id, which
//! `docs/agent-guidance/stalwart-harness.md` forbids. The group mailbox also collects a
//! Sent copy per run of the send test, so it is asserted on by `Message-ID`, never by count.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use std::time::Duration;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageIdHeader, SharedMailboxId},
    mail::{EmailAddress, MailboxAccess, MailboxRole, Message},
    sync::{SyncObject, SyncUpdate},
};
use engine_provider::{Draft, Provider, SharedMailbox, SharedMailboxes};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT, SHARED_MESSAGE_ID};

/// Every `Message-ID` seeded into alice's own folders, none of which may appear in a store
/// that is not hers.
const ALICE_SEED_IDS: &[&str] = &[
    "baseline-0001@test.local",
    "flagged-0005@test.local",
    "thread-root-0006@test.local",
    "thread-reply-0006@test.local",
    "moved-0007@test.local",
    "attachment-0004@test.local",
];

fn account() -> AccountId {
    AccountId::try_from("live-shared").unwrap()
}

fn config(harness: &Harness) -> JmapConfig {
    JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    )
}

fn ready(test: &str) -> Option<Harness> {
    let Some(harness) = Harness::from_env() else {
        eprintln!("skipping {test}: STALWART_HTTP_ADDR unset");
        return None;
    };
    harness
        .wait_until_ready(Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

async fn connect(config: JmapConfig) -> JmapProvider {
    JmapProvider::connect(config).await.expect("connect")
}

/// Discovers `address` with an unbound client, then binds a second one to it — the two
/// halves of onboarding.
async fn bound_to(harness: &Harness, address: &str) -> (SharedMailbox, JmapProvider) {
    let discovery = connect(config(harness)).await;
    let store = discovery
        .resolve_shared_mailbox(address)
        .await
        .unwrap_or_else(|err| panic!("{address} should resolve: {err}"));
    let provider = connect(config(harness).with_shared_mailbox(store.handle.clone())).await;
    (store, provider)
}

fn has_message_id(messages: &[Message], id: &str) -> bool {
    messages
        .iter()
        .any(|message| message.envelope.message_id.iter().any(|m| m.as_str() == id))
}

fn snapshot<T: SyncObject>(update: SyncUpdate<T>) -> Vec<T> {
    let SyncUpdate::Snapshot { objects, .. } = update else {
        panic!("a first sync is a snapshot");
    };
    objects
}

#[tokio::test]
async fn live_the_session_lists_what_is_shared_with_the_credential() {
    let Some(harness) = ready("live_the_session_lists_what_is_shared_with_the_credential") else {
        return;
    };
    let provider = connect(config(&harness)).await;
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );
    let listed = provider.list_shared_mailboxes().await.expect("enumerable");
    let addresses: Vec<&str> = listed.iter().filter_map(|m| m.address.as_deref()).collect();
    let owner = harness.read_only_share_owner().address.clone();
    assert!(addresses.contains(&SHARED_GROUP_ACCOUNT), "{listed:?}");
    assert!(addresses.contains(&owner.as_str()), "{listed:?}");
    // Alice's own account is in the session too, and is not a mailbox shared with her.
    assert!(!addresses.contains(&harness.account.as_str()), "{listed:?}");

    // Resolving by address converges on the handle the list reported.
    let resolved = provider
        .resolve_shared_mailbox(SHARED_GROUP_ACCOUNT)
        .await
        .expect("the group mailbox resolves");
    assert!(listed.iter().any(|m| m.handle == resolved.handle));
    for not_shared in [harness.account.as_str(), "nobody@test.local"] {
        let err = provider
            .resolve_shared_mailbox(not_shared)
            .await
            .unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent, "{not_shared}: {err}");
    }
}

#[tokio::test]
async fn live_a_group_mailbox_syncs_its_own_folders_and_mail() {
    let Some(harness) = ready("live_a_group_mailbox_syncs_its_own_folders_and_mail") else {
        return;
    };
    let (_, shared) = bound_to(&harness, SHARED_GROUP_ACCOUNT).await;

    let folders = snapshot(
        shared
            .sync_mailboxes(&account(), None)
            .await
            .expect("Mailbox/get on the group account")
            .update,
    );
    assert!(
        folders.iter().any(|f| f.role == Some(MailboxRole::Inbox)),
        "{folders:?}"
    );
    // Alice is a member of the group, so she holds every right on its folders.
    assert!(folders.iter().all(|f| f.access == MailboxAccess::owner()));

    let mail = snapshot(
        shared
            .sync_email(&account(), None)
            .await
            .expect("Email sync on the group account")
            .update,
    );
    // The group's seeded message is there, and none of alice's is: one account, one
    // principal's mail.
    assert!(has_message_id(&mail, SHARED_MESSAGE_ID), "{mail:?}");
    for alice_only in ALICE_SEED_IDS {
        assert!(!has_message_id(&mail, alice_only), "{alice_only} leaked in");
    }
}

#[tokio::test]
async fn live_a_read_only_share_says_so_on_its_folder_and_not_its_account() {
    let Some(harness) = ready("live_a_read_only_share_says_so_on_its_folder_and_not_its_account")
    else {
        return;
    };
    let owner = harness.read_only_share_owner().address.clone();
    let (_, shared) = bound_to(&harness, &owner).await;

    // The account flag says writable — asserted rather than assumed, because it is the
    // behaviour the per-folder design rests on.
    assert!(
        shared.connection_info().capabilities.mail_writes(),
        "if Stalwart reports this share's account read-only, revisit modeling.md's rationale"
    );
    let folders = snapshot(
        shared
            .sync_mailboxes(&account(), None)
            .await
            .expect("Mailbox/get on the read-only share")
            .update,
    );
    // Only the one folder the grant covers, and it grants read and nothing else.
    assert_eq!(folders.len(), 1, "{folders:?}");
    assert_eq!(folders[0].access, MailboxAccess::reader());
}

#[tokio::test]
async fn live_a_handle_the_session_no_longer_lists_fails_at_connect() {
    let Some(harness) = ready("live_a_handle_the_session_no_longer_lists_fails_at_connect") else {
        return;
    };
    let stale = SharedMailboxId::try_from("no-such-account").unwrap();
    let Err(err) = JmapProvider::connect(config(&harness).with_shared_mailbox(stale)).await else {
        panic!("a handle the session does not list must not connect");
    };
    assert!(err.to_string().contains("lost access"), "{err}");
}

#[tokio::test]
async fn live_a_client_bound_to_a_group_mailbox_sends_as_it() {
    let Some(harness) = ready("live_a_client_bound_to_a_group_mailbox_sends_as_it") else {
        return;
    };
    let (_, shared) = bound_to(&harness, SHARED_GROUP_ACCOUNT).await;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let message_id = format!("shared-send-{nonce}@test.local");
    let bob = harness.read_only_share_owner().clone();
    let draft = Draft::new(
        MessageIdHeader::new(&message_id).unwrap(),
        EmailAddress::named("Support", SHARED_GROUP_ACCOUNT),
        vec![EmailAddress::new(&bob.address)],
        "Sent as the group mailbox",
        "Sent by the shared-mailbox live suite.",
    );
    shared
        .submit_email(&account(), &draft)
        .await
        .expect("the group's own identity sends as the group");

    // What bob receives says it is from the group — the identity the submission picked was
    // the group's, and the server did not rewrite it.
    let as_bob = connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&bob.address, &bob.password),
    ))
    .await;
    let mut delivered = None;
    for _ in 0..20 {
        let mail = snapshot(as_bob.sync_email(&account(), None).await.unwrap().update);
        delivered = mail.into_iter().find(|message| {
            message
                .envelope
                .message_id
                .iter()
                .any(|m| m.as_str() == message_id)
        });
        if delivered.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let delivered = delivered.expect("bob receives the message");
    assert_eq!(
        delivered
            .envelope
            .from
            .iter()
            .map(|a| a.email.as_str())
            .collect::<Vec<_>>(),
        [SHARED_GROUP_ACCOUNT]
    );
}
