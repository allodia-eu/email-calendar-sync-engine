//! Gated live integration: the shared-mailbox contract, asserted against **every** configured
//! IMAP server — Stalwart (IMAP4rev2) and Dovecot (IMAP4rev1 and IMAP4rev2).
//!
//! The two implementations disagree on nearly every surface detail of sharing, which is the
//! point of running one contract against both:
//!
//! | | Stalwart | Dovecot |
//! |---|---|---|
//! | foreign prefix | `Shared Folders` | `shared/` (trailing delimiter) |
//! | a store's inbox | `…/support@test.local/INBOX`, root `\Noselect` | the root itself, selectable |
//! | a shared path on rev1 | — (rev2 only) | modified UTF-7 |
//! | how rights are read | pipelined `MYRIGHTS`, completed out of order | on the folder `LIST` (RFC 8440) |
//! | full rights | `rliteswkxpa` | `lrwstipekxacd` (with RFC 2086's `c`/`d`) |
//! | roles in a shared store | `\Sent`, `\Drafts`, … as for the owner | none, unless configured |
//!
//! Both put their stores in RFC 2342's *Other Users'* position. Nothing below names either
//! server's prefix: a store's folders are recognised by the handle discovery returned.
//!
//! Skips per server when its address variable is unset, so the offline suite stays green.

#[path = "common/imap_live.rs"]
mod imap_live;

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MessageIdHeader},
    mail::{EmailAddress, MailboxAccess, MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, Provider, SharedMailbox, SharedMailboxes};
use imap_live::{
    FULL_SHARE_OWNER, LiveProvider, READ_ONLY_SHARE_OWNER, SERVERS, Server, connect,
    connect_shared, folders,
};
use stalwart_harness::SHARED_MESSAGE_ID;

/// A seeded `Message-ID` of the account's own, which must never turn up in a shared store.
const OWN_SEED_ID: &str = "baseline-0001@test.local";

async fn discover(provider: &LiveProvider, address: &str) -> SharedMailbox {
    provider
        .resolve_shared_mailbox(address)
        .await
        .unwrap_or_else(|err| panic!("{address} should resolve: {err}"))
}

async fn mail(provider: &LiveProvider) -> Vec<Message> {
    let account = AccountId::try_from("live-harness").unwrap();
    match provider.sync_email(&account, None).await.unwrap().update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { .. } => panic!("a first sync is a snapshot"),
    }
}

fn has(messages: &[Message], id: &str) -> bool {
    messages
        .iter()
        .any(|m| m.envelope.message_id.iter().any(|i| i.as_str() == id))
}

/// The shared store `owner` names on `server`, scoped and listed the way a host does it: a
/// provider bound to a placeholder lists the folders, and the store's own ids come back.
async fn shared_store(
    server: &Server,
    owner: &str,
    test: &str,
) -> Option<(SharedMailbox, Vec<engine_core::mail::Mailbox>)> {
    let own = connect(server, test).await?;
    let store = discover(&own, owner).await;
    let provider = connect_shared(server, &store.handle, "INBOX", test).await?;
    let listed = folders(&provider).await;
    Some((store, listed))
}

#[tokio::test]
async fn both_shares_are_discovered_and_the_credentials_own_is_not_one() {
    let test = "both_shares_are_discovered_and_the_credentials_own_is_not_one";
    for server in &SERVERS {
        let Some(provider) = connect(server, test).await else {
            continue;
        };
        assert_eq!(
            provider.connection_info().capabilities.shared_mailboxes(),
            SharedMailboxes::Enumerable,
            "{}",
            server.label
        );
        let stores = provider.list_shared_mailboxes().await.unwrap();
        let owners: Vec<&str> = stores.iter().filter_map(|s| s.address.as_deref()).collect();
        for owner in [FULL_SHARE_OWNER, READ_ONLY_SHARE_OWNER] {
            assert!(owners.contains(&owner), "{}: {stores:?}", server.label);
            let resolved = discover(&provider, owner).await;
            assert!(stores.iter().any(|s| s.handle == resolved.handle));
        }
        for not_shared in [server.account, "nobody@test.local"] {
            let err = provider
                .resolve_shared_mailbox(not_shared)
                .await
                .unwrap_err();
            assert_eq!(err.class(), FailureClass::Permanent, "{}", server.label);
        }
    }
}

#[tokio::test]
async fn the_credentials_own_folder_list_holds_none_of_a_shared_store() {
    let test = "the_credentials_own_folder_list_holds_none_of_a_shared_store";
    for server in &SERVERS {
        let Some(provider) = connect(server, test).await else {
            continue;
        };
        let handles: Vec<String> = provider
            .list_shared_mailboxes()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.handle.as_str().to_owned())
            .collect();
        let own = folders(&provider).await;
        // Both servers interleave the shares into a flat `LIST` — each under its own prefix.
        for folder in &own {
            assert!(
                handles
                    .iter()
                    .all(|h| !folder.id.as_str().starts_with(h.as_str())),
                "{}: {} belongs to a shared store",
                server.label,
                folder.id.as_str()
            );
        }
        let sent = own.iter().filter(|f| f.role == Some(MailboxRole::Sent));
        assert_eq!(sent.count(), 1, "{}", server.label);
        let inbox = own
            .iter()
            .find(|f| f.role == Some(MailboxRole::Inbox))
            .unwrap();
        assert_eq!(inbox.access, MailboxAccess::owner(), "{}", server.label);
    }
}

#[tokio::test]
async fn a_full_share_reads_like_an_account_of_its_own() {
    let test = "a_full_share_reads_like_an_account_of_its_own";
    for server in &SERVERS {
        let Some((store, listed)) = shared_store(server, FULL_SHARE_OWNER, test).await else {
            continue;
        };
        let label = server.label;
        assert!(
            listed
                .iter()
                .all(|f| f.id.as_str().starts_with(store.handle.as_str())),
            "{label}: {listed:?}"
        );
        let inboxes: Vec<_> = listed
            .iter()
            .filter(|f| f.role == Some(MailboxRole::Inbox))
            .collect();
        // One Inbox, wherever the server keeps it — Stalwart below the root, Dovecot as it.
        assert_eq!(inboxes.len(), 1, "{label}: {listed:?}");
        let inbox = inboxes[0];
        assert_eq!(inbox.parent, None, "{label}");
        assert_eq!(inbox.name, "INBOX", "{label}");
        assert!(
            listed.iter().all(|f| f.access == MailboxAccess::owner()),
            "{label}"
        );

        let bound = connect_shared(server, &store.handle, inbox.id.as_str(), test)
            .await
            .unwrap();
        let messages = mail(&bound).await;
        assert!(has(&messages, SHARED_MESSAGE_ID), "{label}: {messages:?}");
        assert!(!has(&messages, OWN_SEED_ID), "{label}");
    }
}

#[tokio::test]
async fn a_read_only_share_says_so_on_its_one_folder() {
    let test = "a_read_only_share_says_so_on_its_one_folder";
    for server in &SERVERS {
        let Some((_, listed)) = shared_store(server, READ_ONLY_SHARE_OWNER, test).await else {
            continue;
        };
        // The grant covers one INBOX with `lr`. On Dovecot that INBOX is the store's root,
        // so treating a root as a mere container would list nothing at all here.
        assert_eq!(listed.len(), 1, "{}: {listed:?}", server.label);
        assert_eq!(listed[0].role, Some(MailboxRole::Inbox), "{}", server.label);
        assert_eq!(
            listed[0].access,
            MailboxAccess::reader(),
            "{}",
            server.label
        );
    }
}

#[tokio::test]
async fn a_non_ascii_folder_in_a_shared_store_keeps_one_identity() {
    // Dovecot creates `Überweisungen` for every user, the sharer included, so the rev1
    // service lists it as `shared/support@test.local/&ANw-berweisungen`. Its rights and
    // its sync must reach the same folder the id names on either dialect.
    let test = "a_non_ascii_folder_in_a_shared_store_keeps_one_identity";
    for server in &SERVERS {
        let Some((store, listed)) = shared_store(server, FULL_SHARE_OWNER, test).await else {
            continue;
        };
        let Some(folder) = listed.iter().find(|f| f.name == "Überweisungen") else {
            // Stalwart's group mailbox is created with its default folders only.
            continue;
        };
        assert_eq!(folder.access, MailboxAccess::owner(), "{}", server.label);
        let bound = connect_shared(server, &store.handle, folder.id.as_str(), test)
            .await
            .unwrap();
        assert!(mail(&bound).await.is_empty(), "{}", server.label);
    }
}

#[tokio::test]
async fn a_draft_saved_in_a_full_share_lands_in_that_shares_drafts() {
    // Stalwart gives the group mailbox a `\Drafts`; Dovecot's shared namespace carries no
    // SPECIAL-USE attributes, so there the save takes the fallback — the conventional name,
    // qualified by the store, which is the folder Dovecot really has. Either way the draft is
    // the share's, and never lands among the credential's own folders.
    let test = "a_draft_saved_in_a_full_share_lands_in_that_shares_drafts";
    for server in &SERVERS {
        let Some((store, listed)) = shared_store(server, FULL_SHARE_OWNER, test).await else {
            continue;
        };
        let label = server.label;
        let inbox = listed
            .iter()
            .find(|f| f.role == Some(MailboxRole::Inbox))
            .unwrap();
        let shared = connect_shared(server, &store.handle, inbox.id.as_str(), test)
            .await
            .unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let message_id = format!("shared-draft-{nonce}@test.local");
        let draft = Draft::new(
            MessageIdHeader::new(&message_id).unwrap(),
            EmailAddress::new(FULL_SHARE_OWNER),
            vec![EmailAddress::new("nobody@test.local")],
            "A draft in a shared store",
            "Saved by the shared-mailbox contract suite, then removed.",
        );
        let key = shared.save_draft(&draft).await.expect(label);
        assert!(
            key.as_str().contains(store.handle.as_str()),
            "{label}: filed as {}",
            key.as_str()
        );

        let own = connect(server, test).await.unwrap();
        let own_drafts = folders(&own)
            .await
            .into_iter()
            .find(|f| f.role == Some(MailboxRole::Drafts))
            .expect("the credential has a Drafts");
        let own_bound = imap_live::connect_to(server, own_drafts.id.as_str(), test)
            .await
            .unwrap();
        assert!(!has(&mail(&own_bound).await, &message_id), "{label}");

        // Leave the share as it was found, so a rerun sees the same store.
        shared
            .delete_draft(&AccountId::try_from("live-harness").unwrap(), &key)
            .await
            .expect(label);
    }
}
