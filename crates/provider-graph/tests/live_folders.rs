//! Gated live checks on the Graph **folder list**: that a localized display name never
//! decides a role, and that the tree walk reaches folders `/mailFolders` does not list.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set, like every live suite here; see
//! `live_provider.rs` for the command.

mod common;

use std::collections::BTreeSet;

use common::{account, token};
use engine_core::{ids::MailboxId, mail::MailboxRole, sync::SyncUpdate};
use engine_provider::Provider;
use provider_graph::{GraphClient, GraphProvider};

/// A provider bound to the inbox (Graph accepts the well-known alias in the URL). The bound
/// folder is irrelevant to a folder-list sync, which is account-level.
fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

#[tokio::test]
async fn live_mail_folders_resolve_roles() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_folders_resolve_roles: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let mailboxes = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    assert!(mailboxes.is_snapshot());
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    // Roles resolve by well-known-alias id despite localized display names.
    let roles: BTreeSet<MailboxRole> = objects.iter().filter_map(|m| m.role.clone()).collect();
    assert!(roles.contains(&MailboxRole::Inbox), "inbox role resolved");
    assert!(roles.contains(&MailboxRole::Sent), "sent role resolved");
    // A folder the mailbox root holds has no parent (nulled against msgfolderroot).
    let inbox = objects
        .iter()
        .find(|m| m.role == Some(MailboxRole::Inbox))
        .expect("an inbox");
    assert_eq!(inbox.parent, None);
}

/// The nested folders `/mailFolders` does not list, reached through `childFolders`.
///
/// The account carries `Archiveren` → `Fixture parent` → `Fixture child`, two levels deep
/// on purpose: one level is what `$expand=childFolders` can do, and it is not enough. A
/// fake cannot prove this — it answers whatever it is asked — so what the live run adds is
/// that Graph accepts the walk's URL with an immutable folder id inline, and that the
/// second hop really is served.
#[tokio::test]
async fn live_mail_folders_include_the_nested_ones() {
    let Some(token) = token() else {
        eprintln!("skipping live_mail_folders_include_the_nested_ones: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let mailboxes = provider(token)
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    let named = |name: &str| objects.iter().find(|m| m.name == name);
    let archive = named("Archiveren").expect("the archive folder");
    let parent = named("Fixture parent").expect(
        "the nested fixture folder: create `Fixture parent` under Archiveren, and \
         `Fixture child` under it, on the test account",
    );
    let child = named("Fixture child").expect("the twice-nested fixture folder");
    assert_eq!(parent.parent.as_ref(), Some(&archive.id));
    assert_eq!(child.parent.as_ref(), Some(&parent.id));
}
