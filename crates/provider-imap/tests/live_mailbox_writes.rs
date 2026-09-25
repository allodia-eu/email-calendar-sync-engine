//! Gated live integration: changing the folder tree, against every configured IMAP server.
//!
//! The offline suite asserts the commands sent; only a real server says whether it takes
//! them, carries a folder's children through a `RENAME`, and files a trashed folder's
//! mail where the user can get it back. Each change is re-read from the folder list, so a
//! pass cannot be produced by an adapter that sent nothing.
//!
//! Every test works under a root folder of its own, named per run, and removes it at the
//! end. On Stalwart it runs as the scratch account `carol@`, because Alice's folders are
//! count-asserted elsewhere; the Dovecot harness has only Alice, and a folder that is gone
//! again at the end of the test changes none of her counts.
//!
//! Skips per server when its address variable is unset.

#[path = "common/imap_live.rs"]
mod imap_live;

use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader},
    mail::{EmailAddress, Mailbox, MailboxRole},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, MailboxEdit, MailboxEditReceipt, MailboxWrites, Provider};
use imap_live::{DOVECOT_REV1, DOVECOT_REV2, LiveProvider, Server, connect_to, folders};

/// Stalwart, as the scratch account nothing counts.
const STALWART_SCRATCH: Server = Server {
    label: "stalwart",
    addr_var: "STALWART_IMAP_ADDR",
    account: "carol@test.local",
    password: "harness-carol-pw",
};

const SERVERS: [Server; 3] = [STALWART_SCRATCH, DOVECOT_REV1, DOVECOT_REV2];

fn account() -> AccountId {
    AccountId::try_from("live-harness").unwrap()
}

fn id(path: &str) -> MailboxId {
    MailboxId::try_from(path).unwrap()
}

/// A top-level name no earlier run left behind.
fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}{nanos}")
}

async fn edit(provider: &LiveProvider, edit: MailboxEdit, label: &str) -> MailboxEditReceipt {
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .unwrap_or_else(|err| panic!("{edit:?} on {label}: {err}"))
}

fn resolved(receipt: &MailboxEditReceipt) -> MailboxId {
    receipt.mailbox.clone().expect("the edit names a folder")
}

fn listed(all: &[Mailbox], id: &MailboxId) -> Option<Mailbox> {
    all.iter().find(|m| &m.id == id).cloned()
}

/// How many messages the folder at `path` holds, read through a provider bound to it.
async fn count(server: &Server, path: &str, test: &str) -> usize {
    let provider = connect_to(server, path, test).await.expect("configured");
    match provider.sync_email(&account(), None).await {
        Ok(scope) => match scope.update {
            SyncUpdate::Snapshot { objects, .. } => objects.len(),
            SyncUpdate::Delta { .. } => panic!("a first pass is a snapshot"),
        },
        Err(err) => panic!("sync {path} on {}: {err}", server.label),
    }
}

#[tokio::test]
async fn a_folder_is_made_renamed_and_moved_and_its_children_go_with_it() {
    let test = "a_folder_is_made_renamed_and_moved_and_its_children_go_with_it";
    for server in &SERVERS {
        let Some(provider) = connect_to(server, "INBOX", test).await else {
            continue;
        };
        let label = server.label;
        assert!(provider.connection_info().capabilities.mailbox_writes());

        let root = resolved(
            &edit(
                &provider,
                MailboxEdit::Create {
                    name: unique("FolderWrites"),
                    parent: None,
                },
                label,
            )
            .await,
        );
        // A non-ASCII name: modified UTF-7 on the rev1 wire, UTF-8 on rev2.
        let receipts = resolved(
            &edit(
                &provider,
                MailboxEdit::Create {
                    name: "Reçus".into(),
                    parent: Some(root.clone()),
                },
                label,
            )
            .await,
        );
        let inner = resolved(
            &edit(
                &provider,
                MailboxEdit::Create {
                    name: "2024".into(),
                    parent: Some(receipts.clone()),
                },
                label,
            )
            .await,
        );
        let all = folders(&provider).await;
        let made = listed(&all, &receipts).unwrap_or_else(|| panic!("{label} lists {receipts}"));
        assert_eq!(made.name, "Reçus", "{label}");
        assert_eq!(made.parent, Some(root.clone()), "{label}");
        assert!(listed(&all, &inner).is_some(), "{label}");

        // Making it again is the retry case: the same folder, and no error.
        let again = edit(
            &provider,
            MailboxEdit::Create {
                name: "Reçus".into(),
                parent: Some(root.clone()),
            },
            label,
        )
        .await;
        assert_eq!(resolved(&again), receipts, "{label}");

        // Rename: the key moves, and the child moves with it.
        let renamed = resolved(
            &edit(
                &provider,
                MailboxEdit::Update {
                    target: receipts.clone(),
                    name: "Bills".into(),
                    parent: Some(root.clone()),
                },
                label,
            )
            .await,
        );
        let all = folders(&provider).await;
        assert!(
            listed(&all, &receipts).is_none(),
            "{label}: the old path is gone"
        );
        let child = all
            .iter()
            .find(|m| m.parent.as_ref() == Some(&renamed) && m.name == "2024")
            .unwrap_or_else(|| panic!("{label}: the child followed the rename"));
        let child = child.id.clone();

        // Out to the top level, and back in.
        let top_name = unique("FolderWritesTop");
        let top = resolved(
            &edit(
                &provider,
                MailboxEdit::Update {
                    target: renamed.clone(),
                    name: top_name.clone(),
                    parent: None,
                },
                label,
            )
            .await,
        );
        let all = folders(&provider).await;
        let at_top = listed(&all, &top).unwrap_or_else(|| panic!("{label} lists {top}"));
        assert_eq!(at_top.parent, None, "{label}");
        assert!(
            listed(&all, &child).is_none(),
            "{label}: the child moved too"
        );
        assert!(
            all.iter()
                .any(|m| m.parent.as_ref() == Some(&top) && m.name == "2024"),
            "{label}"
        );
        let back = resolved(
            &edit(
                &provider,
                MailboxEdit::Update {
                    target: top.clone(),
                    name: "Bills".into(),
                    parent: Some(root.clone()),
                },
                label,
            )
            .await,
        );
        assert_eq!(back, renamed, "{label}");

        // A folder that is not there cannot be renamed.
        let missing = provider
            .edit_mailbox(
                &account(),
                &MailboxEdit::Update {
                    target: id(&format!("{}-gone", root.as_str())),
                    name: "Anything".into(),
                    parent: None,
                },
            )
            .await
            .expect_err("nothing to rename");
        assert_eq!(missing.class(), FailureClass::Conflict, "{label}");

        edit(
            &provider,
            MailboxEdit::Delete {
                target: root.clone(),
            },
            label,
        )
        .await;
        let all = folders(&provider).await;
        assert!(
            !all.iter().any(|m| m.id.as_str().starts_with(root.as_str())),
            "{label}: the whole subtree is gone"
        );
    }
}

#[tokio::test]
async fn a_trashed_folder_lands_in_trash_with_its_mail_and_a_delete_removes_it_for_good() {
    let test = "a_trashed_folder_lands_in_trash_with_its_mail_and_a_delete_removes_it_for_good";
    for server in &SERVERS {
        let Some(provider) = connect_to(server, "INBOX", test).await else {
            continue;
        };
        let label = server.label;
        let all = folders(&provider).await;
        let Some(trash) = all
            .iter()
            .find(|m| m.role == Some(MailboxRole::Trash))
            .map(|m| m.id.clone())
        else {
            eprintln!("skipping {test} on {label}: no folder carries the Trash role");
            continue;
        };

        let name = unique("Trashed");
        let root = resolved(
            &edit(
                &provider,
                MailboxEdit::Create {
                    name: name.clone(),
                    parent: None,
                },
                label,
            )
            .await,
        );
        let inner = resolved(
            &edit(
                &provider,
                MailboxEdit::Create {
                    name: "Inner".into(),
                    parent: Some(root.clone()),
                },
                label,
            )
            .await,
        );
        // Give the inner folder a message: store it as a draft, then move it in.
        let draft = Draft::new(
            MessageIdHeader::new(format!("{name}@test.local")).unwrap(),
            EmailAddress::new(server.account),
            vec![EmailAddress::new(server.account)],
            "In a folder about to be trashed",
            "Body",
        );
        let key = provider
            .put_draft(&account(), &draft, None)
            .await
            .unwrap_or_else(|err| panic!("store a draft on {label}: {err}"));
        provider
            .edit_mail(&account(), &MailEdit::move_to(key, inner.clone()))
            .await
            .unwrap_or_else(|err| panic!("move it into {inner} on {label}: {err}"));
        assert_eq!(count(server, inner.as_str(), test).await, 1, "{label}");

        let receipt = edit(
            &provider,
            MailboxEdit::Trash {
                target: root.clone(),
                trash: trash.clone(),
                name: name.clone(),
            },
            label,
        )
        .await;
        let all = folders(&provider).await;
        assert!(listed(&all, &root).is_none(), "{label}: gone from the top");
        let in_trash = resolved(&receipt);
        let moved = listed(&all, &in_trash).unwrap_or_else(|| panic!("{label} lists {in_trash}"));
        assert_eq!(moved.parent, Some(trash.clone()), "{label}");
        let moved_inner = all
            .iter()
            .find(|m| m.parent.as_ref() == Some(&in_trash) && m.name == "Inner")
            .unwrap_or_else(|| panic!("{label}: the subfolder went with it"))
            .id
            .clone();
        assert_eq!(
            count(server, moved_inner.as_str(), test).await,
            1,
            "{label}: the mail went with it"
        );

        edit(
            &provider,
            MailboxEdit::Delete {
                target: in_trash.clone(),
            },
            label,
        )
        .await;
        let all = folders(&provider).await;
        assert!(listed(&all, &in_trash).is_none(), "{label}");
        assert!(listed(&all, &moved_inner).is_none(), "{label}");

        // Deleting what is already gone is the retry case, and succeeds.
        let again = edit(&provider, MailboxEdit::Delete { target: in_trash }, label).await;
        assert_eq!(again, MailboxEditReceipt::removed(), "{label}");
    }
}
