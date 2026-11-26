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
        &engine_http::test_server::user_agent(),
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

/// Every folder a well-known alias names carries that alias's role, whatever the folder is
/// called: the standard ones, Exchange's bookkeeping (Sync Issues and the three under it) and
/// its stored chats (Conversation History). The test asks each alias itself, so it asserts
/// per folder rather than per account: an alias the mailbox answers `404` (a personal
/// account has no Sync Issues) or `403` for must leave its role on no folder at all.
///
/// What only a live run shows: that Graph accepts each alias in the URL, and that a mailbox
/// without one answers in a way that leaves the listing whole.
#[tokio::test]
async fn live_mail_folders_carry_the_role_of_each_alias() {
    let Some(token) = token() else {
        eprintln!(
            "skipping live_mail_folders_carry_the_role_of_each_alias: GRAPH_ACCESS_TOKEN unset"
        );
        return;
    };
    let mailboxes = provider(token.clone())
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    let SyncUpdate::Snapshot { objects, .. } = &mailboxes.update else {
        panic!("expected a folder snapshot");
    };
    let http = engine_tls::TlsClientConfig::bundled()
        .reqwest_builder()
        .build()
        .expect("client");
    // Stated here rather than read from the adapter, so the test says what it expects.
    let aliases = [
        ("inbox", MailboxRole::Inbox),
        ("archive", MailboxRole::Archive),
        ("drafts", MailboxRole::Drafts),
        ("sentitems", MailboxRole::Sent),
        ("deleteditems", MailboxRole::Trash),
        ("junkemail", MailboxRole::Junk),
        ("syncissues", MailboxRole::ServerRecords),
        ("conflicts", MailboxRole::ServerRecords),
        ("localfailures", MailboxRole::ServerRecords),
        ("serverfailures", MailboxRole::ServerRecords),
        ("conversationhistory", MailboxRole::ChatHistory),
    ];
    let mut answered = Vec::new();
    for (alias, role) in aliases.iter().cloned() {
        // A GET, as the adapter sends it: ids in their immutable form.
        let reply = http
            .get(format!(
                "https://graph.microsoft.com/v1.0/me/mailFolders/{alias}?$select=id"
            ))
            .bearer_auth(&token)
            .header("Prefer", "IdType=\"ImmutableId\"")
            .send()
            .await
            .expect("alias lookup");
        let status = reply.status().as_u16();
        if status != 200 {
            assert!(matches!(status, 403 | 404), "{alias} answered {status}");
            continue;
        }
        let body: serde_json::Value = reply.json().await.expect("alias JSON");
        let id = body["id"].as_str().expect("an id").to_owned();
        let folder = objects
            .iter()
            .find(|m| m.id.as_str() == id)
            .unwrap_or_else(|| panic!("the {alias} folder is listed"));
        assert_eq!(folder.role.as_ref(), Some(&role), "{alias}");
        answered.push(role);
    }
    // Each role is on exactly the folders its aliases answered with, so a role given to an
    // extra folder shows here.
    for (_, role) in &aliases {
        let carried = objects
            .iter()
            .filter(|m| m.role.as_ref() == Some(role))
            .count();
        let expected = answered.iter().filter(|r| *r == role).count();
        assert_eq!(carried, expected, "{role}");
    }
    // And no folder carries a role this table does not know: an alias the adapter gained
    // without the test hearing of it shows here.
    for folder in objects {
        if let Some(role) = &folder.role {
            assert!(
                aliases.iter().any(|(_, known)| known == role),
                "a folder carries {role}, which no alias here names"
            );
        }
    }
}
