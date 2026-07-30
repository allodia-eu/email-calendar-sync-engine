//! Gated live shared-mailbox checks against the Stalwart harness.
//!
//! The offline suite drives a scripted transcript, so it proves attribution and request
//! *shape*. What it cannot prove is that a real server accepts them: that `NAMESPACE` is
//! answered at all, that a `LIST` pattern containing a space and an `@` matches, that
//! `MYRIGHTS` reports `lr` where an ACL was granted and refuses the namespace container,
//! and that a provider bound to a shared folder reads that principal's mail. Those are here.
//!
//! Per the determinism rule every assertion is on harness-controlled content — folder names,
//! rights letters, a `Message-ID` — never on a server-assigned UID.
//!
//! Skips with no `STALWART_IMAP_ADDR`, so the offline suite stays green.

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Mailbox, MailboxAccess, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider, SharedMailboxes};
use provider_imap::{ImapConfig, ImapProvider};
use stalwart_harness::{Harness, SHARED_GROUP_ACCOUNT, SHARED_MESSAGE_ID};
use tokio_rustls::{TlsConnector, client::TlsStream};

fn account() -> AccountId {
    AccountId::try_from("live-shared").unwrap()
}

/// Accepts the harness's self-signed certificate. Test-only and deliberately insecure.
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
        harness.account.as_str(),
        harness.password.as_str(),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from(mailbox).unwrap(),
    )
    .await
    .expect("connect IMAP")
}

/// Connects as the **peer** — the account that granted the seeded account a read-only ACL —
/// to prove the shared namespace is reported per credential, not per server.
async fn connect_as_peer(harness: &Harness) -> ImapProvider<TlsStream<tokio::net::TcpStream>> {
    let host = harness
        .imap_addr
        .rsplit_once(':')
        .map_or("localhost", |(host, _)| host);
    let peer = harness.read_only_share_owner();
    let config = ImapConfig::new(
        harness.imap_addr.as_str(),
        host,
        peer.address.as_str(),
        peer.password.as_str(),
    );
    ImapProvider::connect(
        &config,
        no_verify_connector(),
        MailboxId::try_from("INBOX").unwrap(),
    )
    .await
    .expect("connect IMAP as the peer")
}

fn ready() -> Option<Harness> {
    let harness = Harness::from_env()?;
    harness
        .wait_until_ready(std::time::Duration::from_secs(30))
        .expect("harness ready");
    Some(harness)
}

async fn folders(provider: &ImapProvider<TlsStream<tokio::net::TcpStream>>) -> Vec<Mailbox> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("folder sync")
        .update
    else {
        panic!("a LIST is always a full snapshot");
    };
    objects
}

#[tokio::test]
async fn live_namespace_discovery_is_per_credential() {
    let Some(harness) = ready() else {
        eprintln!("skipping live_namespace_discovery_is_per_credential: STALWART_IMAP_ADDR unset");
        return;
    };

    // The seeded account has been granted access to two stores, so it can enumerate.
    let alice = connect(&harness, "INBOX").await;
    assert_eq!(
        alice.connection_info().capabilities.shared_mailboxes(),
        SharedMailboxes::Enumerable
    );
    let stores = alice.list_shared_mailboxes().await.expect("enumerable");
    let addresses: Vec<&str> = stores
        .iter()
        .filter_map(|store| store.address.as_deref())
        .collect();
    assert!(
        addresses.contains(&SHARED_GROUP_ACCOUNT),
        "the group mailbox should be discoverable: {stores:?}"
    );
    let peer = harness.read_only_share_owner().address.clone();
    assert!(
        addresses.contains(&peer.as_str()),
        "the peer's shared INBOX should be discoverable: {stores:?}"
    );
    // No entry is the credential's own store: IMAP's personal prefix is the empty string,
    // which is no handle at all.
    assert!(stores.iter().all(|store| !store.personal));

    // Resolving by address answers the same handle enumeration reported.
    let resolved = alice
        .resolve_shared_mailbox(SHARED_GROUP_ACCOUNT)
        .await
        .expect("the shared namespace holds it");
    assert!(resolved.handle.as_str().ends_with(SHARED_GROUP_ACCOUNT));
    assert_eq!(
        alice
            .resolve_shared_mailbox("nobody@test.local")
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );

    // The peer granted access rather than receiving it, so *his* session has no foreign
    // namespace at all — the same server, a different credential, a different answer. This
    // is what makes the capability a fact about the credential.
    let peer_provider = connect_as_peer(&harness).await;
    assert_eq!(
        peer_provider
            .connection_info()
            .capabilities
            .shared_mailboxes(),
        SharedMailboxes::Unsupported
    );
    // And the verb rejects rather than answering an empty list, which a host cannot tell
    // apart from "no shares yet" — the same contract every other adapter keeps.
    assert_eq!(
        peer_provider
            .list_shared_mailboxes()
            .await
            .unwrap_err()
            .class(),
        FailureClass::InvalidState
    );
}

#[tokio::test]
async fn live_the_personal_folder_list_excludes_the_shared_namespace() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_the_personal_folder_list_excludes_the_shared_namespace: \
             STALWART_IMAP_ADDR unset"
        );
        return;
    };
    let provider = connect(&harness, "INBOX").await;
    let folders = folders(&provider).await;

    // Stalwart's flat `LIST "" "*"` returns the shared folders interleaved with the
    // credential's own — that is the gap `NAMESPACE` closes. None may appear here.
    assert!(
        folders
            .iter()
            .all(|folder| !folder.name.contains(SHARED_GROUP_ACCOUNT)),
        "shared folders leaked into the personal list: {:?}",
        folders.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    assert!(folders.iter().any(|folder| folder.name == "INBOX"));

    // There is exactly one `\Sent` folder in view, so filing a sent copy cannot land in
    // another principal's Sent Items.
    let sent: Vec<&Mailbox> = folders
        .iter()
        .filter(|folder| folder.role == Some(MailboxRole::Sent))
        .collect();
    assert_eq!(sent.len(), 1, "exactly one Sent folder: {sent:?}");

    // Her own folders are hers: `MYRIGHTS` reports the full grant.
    assert!(
        folders
            .iter()
            .filter(|folder| folder.role == Some(MailboxRole::Inbox))
            .all(|folder| folder.access == MailboxAccess::owner())
    );
}

#[tokio::test]
async fn live_a_shared_store_syncs_its_own_folders_and_mail() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_a_shared_store_syncs_its_own_folders_and_mail: \
             STALWART_IMAP_ADDR unset"
        );
        return;
    };
    // Discover, then bind — the two halves of onboarding a shared mailbox.
    let discovery = connect(&harness, "INBOX").await;
    let store = discovery
        .resolve_shared_mailbox(SHARED_GROUP_ACCOUNT)
        .await
        .expect("group mailbox");
    let inbox = format!("{}/INBOX", store.handle.as_str());

    let shared = connect(&harness, &inbox).await;
    let folders = folders(&shared).await;
    // Only the group's folders, every one under its root — not one of the credential's own.
    assert!(
        folders
            .iter()
            .all(|folder| folder.name.starts_with(store.handle.as_str())),
        "the shared store's list leaked other folders: {:?}",
        folders.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    // Alice is a member of the group, so she holds every right on its folders.
    let shared_inbox = folders
        .iter()
        .find(|folder| folder.name == inbox)
        .expect("the group mailbox has an INBOX");
    assert_eq!(shared_inbox.access, MailboxAccess::owner());

    // And it is the group's mail, not hers: the one seeded message.
    let SyncUpdate::Snapshot { objects, .. } = shared
        .sync_email(&account(), None)
        .await
        .expect("email sync")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    assert_eq!(objects.len(), 1, "the seeded group message: {objects:?}");
    assert!(
        objects[0]
            .envelope
            .message_id
            .iter()
            .any(|id| id.as_str() == SHARED_MESSAGE_ID),
        "expected {SHARED_MESSAGE_ID}, got {:?}",
        objects[0].envelope.message_id
    );
}

#[tokio::test]
async fn live_a_read_only_share_reports_read_only_rights() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_a_read_only_share_reports_read_only_rights: STALWART_IMAP_ADDR unset"
        );
        return;
    };
    let discovery = connect(&harness, "INBOX").await;
    let peer = harness.read_only_share_owner().address.clone();
    let store = discovery
        .resolve_shared_mailbox(&peer)
        .await
        .expect("the peer's share");
    let inbox = format!("{}/INBOX", store.handle.as_str());

    let shared = connect(&harness, &inbox).await;
    let folders = folders(&shared).await;
    // The ACL grants `lr` on the INBOX alone, so that is all that is in view — and the
    // rights say read-only even though the provider advertises `mail_writes` (any IMAP
    // session can *issue* a `UID STORE`; whether the folder accepts one is the folder's
    // business, and this is where a host learns not to offer it).
    let shared_inbox = folders
        .iter()
        .find(|folder| folder.name == inbox)
        .unwrap_or_else(|| panic!("the shared INBOX should be listed: {folders:?}"));
    assert_eq!(shared_inbox.access, MailboxAccess::reader());
    assert!(shared.connection_info().capabilities.mail_writes());
}

#[tokio::test]
async fn live_a_filing_fallback_never_escapes_the_bound_store() {
    let Some(harness) = ready() else {
        eprintln!(
            "skipping live_a_filing_fallback_never_escapes_the_bound_store: \
             STALWART_IMAP_ADDR unset"
        );
        return;
    };
    // The peer's share is the case that matters: it exposes **one** folder (the INBOX the
    // ACL grants) and therefore advertises no `\Drafts`, so filing a draft there takes the
    // fallback-to-a-conventional-name path.
    //
    // Unqualified, that fallback would `CREATE` and `APPEND` into `Drafts` — which resolves
    // in the *credential's own* namespace, where alice already has one. The save would have
    // succeeded, and another principal's draft would be sitting in her folder. Qualified, it
    // targets `Shared Folders/<peer>/Drafts`, which Stalwart refuses outright
    // (`NO [CANNOT] You are not allowed to create root folders under shared folders.`).
    //
    // So the correct live outcome is a **failure**, and the failure is the proof.
    let discovery = connect(&harness, "INBOX").await;
    let peer = harness.read_only_share_owner().address.clone();
    let store = discovery
        .resolve_shared_mailbox(&peer)
        .await
        .expect("the peer's share");
    let shared = connect(&harness, &format!("{}/INBOX", store.handle.as_str())).await;

    let message_id = "imap-shared-fallback-probe@test.local";
    let draft = Draft::new(
        MessageIdHeader::new(message_id).unwrap(),
        EmailAddress::new(harness.account.as_str()),
        vec![EmailAddress::new("nobody@test.local")],
        "Shared-store filing probe",
        "Never filed anywhere: the fallback folder is the share's, which cannot be created.",
    );
    let err = shared
        .save_draft(&draft)
        .await
        .expect_err("the fallback must target the share, which refuses the CREATE");
    assert!(!err.is_retryable(), "{err}");

    // And nothing landed in the credential's own Drafts — the folder an unqualified
    // fallback would have reached.
    let own_drafts = folders(&discovery)
        .await
        .into_iter()
        .find(|folder| folder.role == Some(MailboxRole::Drafts))
        .expect("alice has her own Drafts");
    let SyncUpdate::Snapshot { objects, .. } = connect(&harness, own_drafts.name.as_str())
        .await
        .sync_email(&account(), None)
        .await
        .expect("sync the credential's own Drafts")
        .update
    else {
        panic!("a first sync is a snapshot");
    };
    assert!(
        !objects.iter().any(|message| {
            message
                .envelope
                .message_id
                .iter()
                .any(|id| id.as_str() == message_id)
        }),
        "the shared store's draft leaked into the credential's own Drafts: {objects:?}"
    );
}
