//! Gated live integration: changing the folder tree with `Mailbox/set` against the Stalwart
//! harness.
//!
//! The offline suite asserts the request each edit sends; only a real server says whether
//! it is one Stalwart accepts, and what it does to the mail inside. Each assertion reads the
//! tree back through the adapter's own folder sync, so an edit that sent nothing (or that
//! Stalwart ignored) fails here rather than passing.
//!
//! Runs as the scratch account `carol@`, whose mailboxes no suite counts, under folder names
//! unique to this run, and removes what it made. Skips with no `STALWART_HTTP_ADDR`.

use core::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::{
    error::FailureClass,
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Mailbox, MailboxRole, Message},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, MailboxEdit, MailboxWrites, Provider};
use provider_jmap::{Credentials, JmapConfig, JmapProvider};
use stalwart_harness::Harness;

fn account() -> AccountId {
    AccountId::try_from("live-mailbox-writes").unwrap()
}

/// A prefix no other run or test shares, so concurrent tests never meet each other's folders.
fn unique(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("live-jmap-{label}-{nanos}")
}

async fn connect(harness: &Harness) -> JmapProvider {
    let carol = &harness.scratch[1];
    JmapProvider::connect(JmapConfig::new(
        format!("http://{}", harness.http_addr),
        Credentials::basic(&carol.address, &carol.password),
    ))
    .await
    .expect("connect")
}

async fn folders(provider: &JmapProvider) -> Vec<Mailbox> {
    let SyncUpdate::Snapshot { objects, .. } = provider
        .sync_mailboxes(&account(), None)
        .await
        .unwrap()
        .update
    else {
        panic!("a cursorless folder sync is a snapshot");
    };
    objects
}

async fn folder(provider: &JmapProvider, id: &MailboxId) -> Option<Mailbox> {
    folders(provider).await.into_iter().find(|m| &m.id == id)
}

async fn messages(provider: &JmapProvider) -> Vec<Message> {
    let SyncUpdate::Snapshot { objects, .. } =
        provider.sync_email(&account(), None).await.unwrap().update
    else {
        panic!("a cursorless sync is a snapshot");
    };
    objects
}

async fn create(provider: &JmapProvider, name: &str, parent: Option<&MailboxId>) -> MailboxId {
    let edit = MailboxEdit::Create {
        name: name.into(),
        parent: parent.cloned(),
    };
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .expect("create")
        .mailbox
        .expect("a create names its folder")
}

async fn place(
    provider: &JmapProvider,
    target: &MailboxId,
    name: &str,
    parent: Option<&MailboxId>,
) -> Result<(), FailureClass> {
    let edit = MailboxEdit::Update {
        target: target.clone(),
        name: name.into(),
        parent: parent.cloned(),
    };
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .map(|_| ())
        .map_err(|e| e.class())
}

async fn delete(provider: &JmapProvider, target: &MailboxId) {
    let edit = MailboxEdit::Delete {
        target: target.clone(),
    };
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .expect("delete");
}

/// Files one message in `folder`, returning its key: saved as a draft, then moved.
async fn file_message(provider: &JmapProvider, folder: &MailboxId, label: &str) -> ProviderKey {
    let draft = Draft::new(
        MessageIdHeader::new(format!("{label}@test.local")).unwrap(),
        EmailAddress::new("carol@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        label,
        "Filed by the live folder test.",
    );
    let key = provider
        .put_draft(&account(), &draft, None)
        .await
        .expect("draft");
    provider
        .edit_mail(&account(), &MailEdit::move_to(key.clone(), folder.clone()))
        .await
        .expect("move");
    key
}

async fn trash(provider: &JmapProvider) -> MailboxId {
    folders(provider)
        .await
        .into_iter()
        .find(|m| m.role == Some(MailboxRole::Trash))
        .expect("carol has a Trash")
        .id
}

#[tokio::test]
async fn folders_are_made_renamed_moved_and_refused_on_a_real_server() {
    let Some(harness) = Harness::from_env() else {
        return;
    };
    harness.wait_until_ready(Duration::from_secs(30)).unwrap();
    let provider = connect(&harness).await;
    assert!(provider.connection_info().capabilities.mailbox_writes());

    let root_name = unique("tree");
    let root = create(&provider, &root_name, None).await;
    let child = create(&provider, "Überweisungen 2024", Some(&root)).await;
    let stored = folder(&provider, &child)
        .await
        .expect("the child is listed");
    assert_eq!(stored.name, "Überweisungen 2024");
    assert_eq!(stored.parent.as_ref(), Some(&root));
    assert!(stored.subscribed);

    // A retried create meets the folder the first attempt made.
    assert_eq!(create(&provider, &root_name, None).await, root);

    // Rename, then out to the top, then back in: the id survives every step.
    place(&provider, &child, "Q1", Some(&root)).await.unwrap();
    assert_eq!(folder(&provider, &child).await.unwrap().name, "Q1");
    let top_name = unique("moved");
    place(&provider, &child, &top_name, None).await.unwrap();
    assert_eq!(folder(&provider, &child).await.unwrap().parent, None);
    place(&provider, &child, "Q1", Some(&root)).await.unwrap();
    assert_eq!(
        folder(&provider, &child).await.unwrap().parent.as_ref(),
        Some(&root)
    );

    // A sibling holding the name, a missing target, and a move inside itself all refuse.
    create(&provider, "Taken", Some(&root)).await;
    assert_eq!(
        place(&provider, &child, "Taken", Some(&root)).await,
        Err(FailureClass::Conflict)
    );
    let missing = MailboxId::try_from("no-such-mailbox").unwrap();
    assert_eq!(
        place(&provider, &missing, "x", None).await,
        Err(FailureClass::Conflict)
    );
    assert_eq!(
        place(&provider, &root, &root_name, Some(&child)).await,
        Err(FailureClass::Conflict)
    );

    delete(&provider, &root).await;
    assert!(folder(&provider, &root).await.is_none());
    assert!(folder(&provider, &child).await.is_none());
    // Deleting it again is the retry of a delete that landed.
    delete(&provider, &root).await;
}

#[tokio::test]
async fn a_trashed_folder_keeps_its_mail_and_a_deleted_one_takes_it_along() {
    let Some(harness) = Harness::from_env() else {
        return;
    };
    harness.wait_until_ready(Duration::from_secs(30)).unwrap();
    let provider = connect(&harness).await;
    let trash = trash(&provider).await;

    let name = unique("bills");
    let bills = create(&provider, &name, None).await;
    let inner = create(&provider, "2024", Some(&bills)).await;
    let kept = file_message(&provider, &inner, &unique("kept")).await;

    let edit = MailboxEdit::Trash {
        target: bills.clone(),
        trash: trash.clone(),
        name: name.clone(),
    };
    let receipt = provider.edit_mailbox(&account(), &edit).await.unwrap();
    assert_eq!(receipt.mailbox.as_ref(), Some(&bills));

    let trashed = folder(&provider, &bills).await.expect("still a folder");
    assert_eq!(trashed.parent.as_ref(), Some(&trash));
    assert_eq!(
        folder(&provider, &inner).await.unwrap().parent.as_ref(),
        Some(&bills),
        "the subfolder travels with it"
    );
    let mail = messages(&provider).await;
    let message = mail.iter().find(|m| m.id.key() == &kept).expect("kept");
    assert!(message.mailboxes.contains(&inner));

    // Deleting from Trash takes the subtree and its mail for good.
    delete(&provider, &bills).await;
    assert!(folder(&provider, &bills).await.is_none());
    assert!(folder(&provider, &inner).await.is_none());
    assert!(
        !messages(&provider)
            .await
            .iter()
            .any(|m| m.id.key() == &kept),
        "the message went with its folder"
    );
}
