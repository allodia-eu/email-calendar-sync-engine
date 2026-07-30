//! Gated live shared-mailbox checks against the Stalwart harness.
//!
//! The offline suite drives a captured session document, so it proves the *parsing*. What
//! it cannot prove is that a real server hands back three accounts, that `Mailbox/get` with
//! a shared `accountId` is accepted rather than rejected, or that the mail inside a store
//! the credential does not own actually comes back. Those are what this file checks.
//!
//! Every account id here is resolved **by name**: Stalwart assigns them in creation order,
//! so they shift whenever the fixture is rebuilt (they did, mid-development). A test that
//! hard-coded one would be asserting on a server-assigned id — the thing
//! `docs/agent-guidance/stalwart-harness.md` forbids.
//!
//! Skips with no `STALWART_HTTP_ADDR`, so the offline suite stays green.

use engine_core::{error::FailureClass, ids::AccountId, mail::MailboxAccess, sync::SyncUpdate};
use engine_provider::{Provider, SharedMailbox, SharedMailboxes};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT, SHARED_MESSAGE_ID};

fn account() -> AccountId {
    AccountId::try_from("live-shared").unwrap()
}

fn config(harness: &Harness) -> JmapConfig {
    JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&harness.account, &harness.password),
    )
}

fn ready() -> Option<Harness> {
    let harness = Harness::from_env()?;
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

fn by_address<'a>(listed: &'a [SharedMailbox], address: &str) -> &'a SharedMailbox {
    listed
        .iter()
        .find(|mailbox| mailbox.address.as_deref() == Some(address))
        .unwrap_or_else(|| panic!("no shared mailbox for {address}: {listed:?}"))
}

#[tokio::test]
async fn live_session_enumerates_the_shared_accounts() {
    let Some(harness) = ready() else {
        eprintln!("skipping live_session_enumerates_the_shared_accounts: STALWART_HTTP_ADDR unset");
        return;
    };
    let provider = JmapProvider::connect(config(&harness))
        .await
        .expect("connect");

    // Enumeration is free on this protocol: the accounts map came with the session that
    // the connect above already fetched, so the capability is a fact about the credential.
    assert_eq!(
        provider.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );

    let listed = provider.list_shared_mailboxes().await.expect("enumerable");
    assert_eq!(
        listed.len(),
        3,
        "alice's own store plus the two shared with her: {listed:?}"
    );
    assert!(by_address(&listed, &harness.account).personal);
    assert!(!by_address(&listed, SHARED_GROUP_ACCOUNT).personal);
    assert!(!by_address(&listed, &harness.read_only_share_owner().address).personal);

    // Resolving by address answers with the same handle enumeration reported, so a host
    // that asked the user to type an address and one that offered a list converge.
    let resolved = provider
        .resolve_shared_mailbox(SHARED_GROUP_ACCOUNT)
        .await
        .expect("the session lists the group mailbox");
    assert_eq!(
        resolved.handle,
        by_address(&listed, SHARED_GROUP_ACCOUNT).handle
    );

    // An address the credential has not been granted is absent from the session, and the
    // classification says so terminally rather than inviting a retry.
    assert_eq!(
        provider
            .resolve_shared_mailbox("nobody@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
}

#[tokio::test]
async fn live_shared_group_mailbox_syncs_its_own_mail() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_shared_group_mailbox_syncs_its_own_mail: STALWART_HTTP_ADDR unset"
        );
        return;
    };
    let discovery = JmapProvider::connect(config(&harness))
        .await
        .expect("connect");
    let group = discovery
        .resolve_shared_mailbox(SHARED_GROUP_ACCOUNT)
        .await
        .expect("group mailbox");

    // Bind a second provider to the discovered handle — the whole binding step, and the
    // point of the design: from here on this is an ordinary account.
    let shared = JmapProvider::connect(config(&harness).with_account(group.handle.clone()))
        .await
        .expect("connect bound to the group mailbox");

    let SyncUpdate::Snapshot {
        objects: mailboxes, ..
    } = shared
        .sync_mailboxes(&account(), None)
        .await
        .expect("Mailbox/get on the shared account")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    let inbox = mailboxes
        .iter()
        .find(|mailbox| mailbox.role == Some(engine_core::mail::MailboxRole::Inbox))
        .expect("the group mailbox has an Inbox");
    // Alice is a *member* of the group, so she holds every right on its folders.
    assert_eq!(inbox.access, MailboxAccess::owner());

    // And its mail is the group's own, not alice's: the one seeded message, which exists
    // in no folder of hers.
    let SyncUpdate::Snapshot {
        objects: messages, ..
    } = shared
        .sync_email(&account(), None)
        .await
        .expect("Email sync on the shared account")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(messages.len(), 1, "the seeded group message: {messages:?}");
    assert!(
        messages[0]
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == SHARED_MESSAGE_ID),
        "expected {SHARED_MESSAGE_ID}, got {:?}",
        messages[0].envelope.message_id
    );
}

#[tokio::test]
async fn live_a_read_only_share_reports_read_only_rights_on_its_mailbox() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_a_read_only_share_reports_read_only_rights_on_its_mailbox: \
             STALWART_HTTP_ADDR unset"
        );
        return;
    };
    let discovery = JmapProvider::connect(config(&harness))
        .await
        .expect("connect");
    let peer = discovery
        .resolve_shared_mailbox(&harness.read_only_share_owner().address)
        .await
        .expect("the read-only share");

    let shared = JmapProvider::connect(config(&harness).with_account(peer.handle))
        .await
        .expect("connect bound to the read-only share");

    // The account says writable — this is the live behaviour the whole design rests on, so
    // it is asserted rather than assumed.
    assert!(
        shared.connection_info().capabilities.mail_writes(),
        "if Stalwart ever reports this share's account read-only, the rationale for \
         per-mailbox rights in modeling.md is worth revisiting"
    );

    let SyncUpdate::Snapshot {
        objects: mailboxes, ..
    } = shared
        .sync_mailboxes(&account(), None)
        .await
        .expect("Mailbox/get on the read-only share")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    // Only the one folder the ACL exposes, and it grants read and nothing else. The
    // mailbox is where the truth is.
    assert_eq!(mailboxes.len(), 1, "only the shared INBOX: {mailboxes:?}");
    assert_eq!(mailboxes[0].access, MailboxAccess::reader());
}
