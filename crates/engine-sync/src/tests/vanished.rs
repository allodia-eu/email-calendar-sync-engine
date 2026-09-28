//! A folder that leaves the folder list takes its mail out of the store with it.

use super::*;

fn imap_scope(folder: &str) -> SyncScope {
    SyncScope::ImapMailbox {
        account: account(),
        mailbox: MailboxId::try_from(folder).unwrap(),
    }
}

/// One provider per folder over a list of exactly `listed`, the IMAP shape.
fn pass(listed: &[&str], synced: &[&str]) -> Vec<FakeMail> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let boxes: Vec<Mailbox> = listed
        .iter()
        .map(|name| mailbox(name, name, (*name == "INBOX").then_some(MailboxRole::Inbox)))
        .collect();
    synced
        .iter()
        .map(|name| {
            FakeMail::new(boxes.clone(), vec![message("m", name, "Hi")]).in_folder(name, &log)
        })
        .collect()
}

async fn sync(providers: &[FakeMail], store: &SqliteStore<ManualClock>) -> crate::MailSyncReport {
    sync_mail(
        providers,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        StreamTuning::new(0, 0),
        &IgnoreCommits,
    )
    .await
}

#[tokio::test]
async fn a_renamed_folder_leaves_no_copy_of_its_mail_under_the_old_name() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let first = sync(&pass(&["INBOX", "Old"], &["INBOX", "Old"]), &store).await;
    assert!(first.is_ok(), "{first:?}");
    assert_eq!(
        store.object_keys(&imap_scope("Old")).await.unwrap().len(),
        1
    );

    // On IMAP a rename is a new path: the list names `New`, and nothing names `Old`.
    let second = sync(&pass(&["INBOX", "New"], &["INBOX", "New"]), &store).await;
    assert!(second.is_ok(), "{second:?}");

    assert!(
        store
            .object_keys(&imap_scope("Old"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .load_sync_state(account(), &imap_scope("Old"))
            .await
            .unwrap()
            .is_none(),
        "a folder that comes back under the old name starts over rather than resuming"
    );
    assert_eq!(
        store.object_keys(&imap_scope("New")).await.unwrap().len(),
        1
    );
    assert_eq!(
        store.object_keys(&imap_scope("INBOX")).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn a_folder_list_that_arrives_empty_forgets_nothing() {
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    sync(&pass(&["INBOX", "Old"], &["INBOX", "Old"]), &store).await;

    let report = sync(&pass(&[], &["Old"]), &store).await;
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(
        store.object_keys(&imap_scope("Old")).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn an_account_wide_message_scope_is_never_forgotten() {
    // JMAP and Gmail keep one message scope for the account; it names no folder.
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let boxes = vec![mailbox("inbox", "Inbox", Some(MailboxRole::Inbox))];
    let provider = FakeMail::new(boxes, vec![message("m", "inbox", "Hi")]);
    sync(core::slice::from_ref(&provider), &store).await;

    let forgotten = crate::forget_vanished_folders(
        &store,
        &account(),
        &LeaseRequest::new(worker(), Duration::from_mins(1)),
    )
    .await
    .unwrap();
    assert_eq!(forgotten, 0);
    assert_eq!(
        store
            .object_keys(&provider.email_scope(&account()))
            .await
            .unwrap()
            .len(),
        1
    );
}
