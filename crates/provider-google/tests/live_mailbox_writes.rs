//! Gated live checks for changing Gmail labels as folders: create, rename, move, trash and
//! delete, against a real account.
//!
//! What only a live call shows: that a nested label is found under its parent once the adapter
//! has made it, that a rename carries the labels beneath it (Gmail nests by name alone), and
//! that trashing a folder leaves a message filed elsewhere where it was. The branch that sends
//! mail to Trash cannot be driven from here: the only way this suite can make a message is to
//! send one to itself, and a sent message carries `SENT`, which is a placement of its own.
//!
//! Every label is uniquely named and removed again; the probe message is deleted.
//!
//! Skips unless `GOOGLE_ACCESS_TOKEN` is set:
//!
//! ```sh
//! GOOGLE_ACCESS_TOKEN="$(cargo run -q --manifest-path tools/google-oauth/Cargo.toml -- token)" \
//!   cargo test -p provider-google --test live_mailbox_writes -- --nocapture --test-threads=1
//! ```

use engine_core::{
    ids::{AccountId, MailboxId, MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Mailbox},
    sync::SyncUpdate,
};
use engine_provider::{Draft, MailEdit, MailboxEdit, MailboxWrites, Provider};
use provider_google::{GmailProvider, GoogleClient};

/// The test account's own address; the probe is self-addressed, so nothing leaves the mailbox.
const SELF_ADDRESS: &str = "allodia.e2e@gmail.com";

fn account() -> AccountId {
    AccountId::try_from("live").unwrap()
}

fn token() -> Option<String> {
    std::env::var("GOOGLE_ACCESS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

fn provider(token: String) -> GmailProvider {
    let client = GoogleClient::connect(
        token,
        &engine_tls::TlsClientConfig::bundled(),
        &engine_http::RetryConfig::default(),
    )
    .expect("client");
    GmailProvider::new(client)
}

async fn folders(provider: &GmailProvider) -> Vec<Mailbox> {
    match provider
        .sync_mailboxes(&account(), None)
        .await
        .expect("labels")
        .update
    {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    }
}

fn find<'a>(list: &'a [Mailbox], id: &MailboxId) -> Option<&'a Mailbox> {
    list.iter().find(|m| &m.id == id)
}

async fn apply(provider: &GmailProvider, edit: MailboxEdit) -> Option<MailboxId> {
    provider
        .edit_mailbox(&account(), &edit)
        .await
        .unwrap_or_else(|err| panic!("{edit:?}: {err}"))
        .mailbox
}

async fn membership(provider: &GmailProvider, key: &ProviderKey) -> Vec<String> {
    let snapshot = provider
        .sync_email(&account(), None)
        .await
        .expect("snapshot");
    let SyncUpdate::Snapshot { objects, .. } = &snapshot.update else {
        panic!("expected a snapshot");
    };
    objects
        .iter()
        .find(|message| message.id.key() == key)
        .map(|message| {
            message
                .mailboxes
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn labels_behave_as_folders_across_create_rename_move_trash_and_delete() {
    let Some(token) = token() else {
        eprintln!("skipping live Gmail folder test: GOOGLE_ACCESS_TOKEN unset");
        return;
    };
    let provider = provider(token);
    assert!(provider.connection_info().capabilities.mailbox_writes());
    let marker = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let top = format!("Allodia Probe {marker}");

    // --- create a folder and one inside it -------------------------------------------
    let parent = apply(
        &provider,
        MailboxEdit::Create {
            name: top.clone(),
            parent: None,
        },
    )
    .await
    .expect("a create names its label");
    let child = apply(
        &provider,
        MailboxEdit::Create {
            name: "Child".into(),
            parent: Some(parent.clone()),
        },
    )
    .await
    .expect("a create names its label");
    let list = folders(&provider).await;
    let made = find(&list, &child).expect("the child is listed");
    assert_eq!(made.parent.as_ref(), Some(&parent));
    assert_eq!(made.name, "Child");

    // A second create of the same folder is the first one.
    let again = apply(
        &provider,
        MailboxEdit::Create {
            name: "Child".into(),
            parent: Some(parent.clone()),
        },
    )
    .await;
    assert_eq!(again.as_ref(), Some(&child));

    // --- rename the parent: the child follows ----------------------------------------
    let renamed = format!("{top} Renamed");
    apply(
        &provider,
        MailboxEdit::Update {
            target: parent.clone(),
            name: renamed.clone(),
            parent: None,
        },
    )
    .await;
    let list = folders(&provider).await;
    assert_eq!(find(&list, &parent).unwrap().name, renamed);
    assert_eq!(find(&list, &child).unwrap().parent.as_ref(), Some(&parent));

    // --- move the child to the top and back ------------------------------------------
    let moved = format!("{top} Moved");
    apply(
        &provider,
        MailboxEdit::Update {
            target: child.clone(),
            name: moved.clone(),
            parent: None,
        },
    )
    .await;
    let list = folders(&provider).await;
    let at_top = find(&list, &child).unwrap();
    assert_eq!(
        (at_top.parent.as_ref(), at_top.name.as_str()),
        (None, moved.as_str())
    );
    apply(
        &provider,
        MailboxEdit::Update {
            target: child.clone(),
            name: "Child".into(),
            parent: Some(parent.clone()),
        },
    )
    .await;
    assert_eq!(
        find(&folders(&provider).await, &child)
            .unwrap()
            .parent
            .as_ref(),
        Some(&parent)
    );

    // --- trash the parent: a sent message filed in the child stays where it was --------
    let draft = Draft::new(
        MessageIdHeader::new(format!("gmail-folders-{marker}@example.test")).unwrap(),
        EmailAddress::new(SELF_ADDRESS),
        vec![EmailAddress::new(SELF_ADDRESS)],
        format!("Live folder probe {marker}"),
        "Folder probe body.",
    );
    let key = provider
        .submit_email(&account(), &draft)
        .await
        .expect("send the probe")
        .email_key;
    provider
        .edit_mail(&account(), &MailEdit::move_to(key.clone(), child.clone()))
        .await
        .expect("file the probe in the child");
    let trashed = apply(
        &provider,
        MailboxEdit::Trash {
            target: parent.clone(),
            trash: MailboxId::try_from("TRASH").unwrap(),
            name: renamed.clone(),
        },
    )
    .await;
    assert_eq!(trashed, None, "Gmail cannot keep a label inside Trash");
    let list = folders(&provider).await;
    assert!(find(&list, &parent).is_none() && find(&list, &child).is_none());
    let labels = membership(&provider, &key).await;
    assert!(labels.contains(&"SENT".to_owned()), "{labels:?}");
    assert!(!labels.contains(&"TRASH".to_owned()), "{labels:?}");
    assert!(!labels.contains(&child.as_str().to_owned()), "{labels:?}");

    // --- delete: a label already gone is removed --------------------------------------
    assert_eq!(
        apply(&provider, MailboxEdit::Delete { target: parent }).await,
        None
    );

    if let Err(err) = provider.edit_mail(&account(), &MailEdit::delete(key)).await {
        eprintln!("cleanup delete gave up (leaving the probe): {err}");
    }
}
