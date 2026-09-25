//! Gated live checks on Graph **folder writes**: create (top level and nested), rename,
//! move to the top and back, a repeated create, trash into Deleted Items, and a permanent
//! delete, each read back through the folder-list sync the host sees.
//!
//! Skips unless `GRAPH_ACCESS_TOKEN` is set, like every live suite here; see
//! `live_provider.rs` for the command. Every folder it makes carries a per-run name and is
//! removed at the end, whatever happened in between.

mod common;

use common::{account, token};
use engine_core::{
    ids::MailboxId,
    mail::{Mailbox, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{MailboxEdit, MailboxWrites, Provider};
use provider_graph::{GraphClient, GraphProvider};

fn provider(token: String) -> GraphProvider {
    let client = GraphClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GraphProvider::new(client, MailboxId::try_from("inbox").unwrap())
}

async fn folders(provider: &GraphProvider) -> Vec<Mailbox> {
    let sync = provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("sync folders");
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected a folder snapshot");
    };
    objects
}

async fn find(provider: &GraphProvider, id: &MailboxId) -> Option<Mailbox> {
    folders(provider).await.into_iter().find(|m| &m.id == id)
}

async fn edit(provider: &GraphProvider, edit: MailboxEdit) -> Option<MailboxId> {
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .unwrap_or_else(|err| panic!("{edit:?}: {err}"))
        .mailbox
}

#[tokio::test]
async fn live_folder_tree_writes_round_trip() {
    let Some(token) = token() else {
        eprintln!("skipping live_folder_tree_writes_round_trip: GRAPH_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let parent_name = format!("Engine live {run}");

    let parent = edit(
        &provider,
        MailboxEdit::Create {
            name: parent_name.clone(),
            parent: None,
        },
    )
    .await
    .expect("a create names its folder");
    let outcome = exercise(&provider, &parent, &parent_name).await;

    // Clean up whatever the run left, then report.
    let _ = provider
        .edit_mailbox(
            &account(),
            &MailboxEdit::Delete {
                target: parent.clone(),
            },
        )
        .await;
    assert!(
        find(&provider, &parent).await.is_none(),
        "cleanup removed the folder"
    );
    outcome.unwrap();
}

async fn exercise(
    provider: &GraphProvider,
    parent: &MailboxId,
    parent_name: &str,
) -> Result<(), String> {
    let stored = find(provider, parent)
        .await
        .ok_or("the new folder is listed")?;
    assert_eq!(stored.name, parent_name);
    assert_eq!(stored.parent, None, "a top-level folder has no parent");

    // A second create of the same name is the retry of the first: the same folder.
    let again = edit(
        provider,
        MailboxEdit::Create {
            name: parent_name.to_owned(),
            parent: None,
        },
    )
    .await;
    assert_eq!(again.as_ref(), Some(parent));

    let child = edit(
        provider,
        MailboxEdit::Create {
            name: "Child".into(),
            parent: Some(parent.clone()),
        },
    )
    .await
    .ok_or("a nested create names its folder")?;
    assert_eq!(
        find(provider, &child).await.unwrap().parent.as_ref(),
        Some(parent)
    );

    let renamed = edit(
        provider,
        MailboxEdit::Update {
            target: child.clone(),
            name: "Renamed".into(),
            parent: Some(parent.clone()),
        },
    )
    .await;
    assert_eq!(
        renamed.as_ref(),
        Some(&child),
        "a folder id survives a rename"
    );
    assert_eq!(find(provider, &child).await.unwrap().name, "Renamed");

    let top = edit(
        provider,
        MailboxEdit::Update {
            target: child.clone(),
            name: "Renamed".into(),
            parent: None,
        },
    )
    .await;
    assert_eq!(top.as_ref(), Some(&child), "a folder id survives a move");
    assert_eq!(find(provider, &child).await.unwrap().parent, None);
    edit(
        provider,
        MailboxEdit::Update {
            target: child.clone(),
            name: "Renamed".into(),
            parent: Some(parent.clone()),
        },
    )
    .await;
    assert_eq!(
        find(provider, &child).await.unwrap().parent.as_ref(),
        Some(parent)
    );

    let trash = folders(provider)
        .await
        .into_iter()
        .find(|m| m.role == Some(MailboxRole::Trash))
        .ok_or("the account has Deleted Items")?;
    let trashed = edit(
        provider,
        MailboxEdit::Trash {
            target: parent.clone(),
            trash: trash.id.clone(),
            name: format!("{parent_name} trashed"),
        },
    )
    .await;
    assert_eq!(trashed.as_ref(), Some(parent));
    let in_trash = find(provider, parent).await.unwrap();
    assert_eq!(in_trash.parent.as_ref(), Some(&trash.id));
    assert_eq!(in_trash.name, format!("{parent_name} trashed"));
    assert!(
        find(provider, &child).await.is_some(),
        "the subfolder went into Trash with it"
    );

    let removed = edit(
        provider,
        MailboxEdit::Delete {
            target: parent.clone(),
        },
    )
    .await;
    assert_eq!(removed, None);
    assert!(find(provider, parent).await.is_none());
    assert!(find(provider, &child).await.is_none());
    // Deleting it again is the retry of a delete that landed.
    assert_eq!(
        edit(
            provider,
            MailboxEdit::Delete {
                target: parent.clone()
            }
        )
        .await,
        None
    );
    Ok(())
}
