//! Folder-tree changes through the outbox: sent when the folder is where the user saw it,
//! parked while offline, refused on replay when it changed elsewhere, never repeated.

use engine_provider::{MailboxEdit, MailboxEditReceipt, MailboxWrites};

use super::*;
use crate::{MailboxChange, MailboxPlace, edit_mailbox};

/// A server's folder tree with ids that survive a rename (the JMAP, Graph and Gmail shape).
struct Tree {
    folders: Mutex<Vec<Mailbox>>,
    /// Calls left to refuse as unreachable: the outage a queued change waits out.
    offline: Mutex<u32>,
    /// Every edit that reached the server, in order.
    sent: Mutex<Vec<MailboxEdit>>,
    next_id: Mutex<u32>,
}

impl Tree {
    fn new() -> Self {
        Self {
            folders: Mutex::new(vec![
                mailbox("inbox", "Inbox", Some(MailboxRole::Inbox)),
                mailbox("trash", "Trash", Some(MailboxRole::Trash)),
                mailbox("work", "Work", None),
            ]),
            offline: Mutex::new(0),
            sent: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        }
    }

    fn offline_for(self, calls: u32) -> Self {
        *self.offline.lock().unwrap() = calls;
        self
    }

    fn unreachable(&self) -> bool {
        let mut left = self.offline.lock().unwrap();
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }

    fn sent(&self) -> Vec<MailboxEdit> {
        self.sent.lock().unwrap().clone()
    }

    fn folder(&self, id: &str) -> Option<Mailbox> {
        let id = MailboxId::try_from(id).unwrap();
        self.folders
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned()
    }

    /// Another client renames a folder.
    fn rename_elsewhere(&self, id: &str, name: &str) {
        let id = MailboxId::try_from(id).unwrap();
        let mut folders = self.folders.lock().unwrap();
        folders.iter_mut().find(|m| m.id == id).unwrap().name = name.into();
    }

    fn place(&self, target: &MailboxId, name: &str, parent: Option<&MailboxId>) {
        let mut folders = self.folders.lock().unwrap();
        let folder = folders.iter_mut().find(|m| &m.id == target).unwrap();
        folder.name = name.into();
        folder.parent = parent.cloned();
    }
}

#[async_trait::async_trait]
impl Provider for Tree {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_mailbox_writes())
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        if self.unreachable() {
            return Err(ProviderError::retryable("no network"));
        }
        let folders = self.folders.lock().unwrap().clone();
        let present = folders.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(folders, present),
            SyncState::new("tree"),
        ))
    }
}

impl CalendarWrites for Tree {}

#[async_trait::async_trait]
impl MailboxWrites for Tree {
    async fn edit_mailbox(
        &self,
        _account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        self.sent.lock().unwrap().push(edit.clone());
        Ok(match edit {
            MailboxEdit::Create { name, parent } => {
                let mut next = self.next_id.lock().unwrap();
                let id = format!("new-{next}");
                *next += 1;
                let mut created = mailbox(&id, name, None);
                created.parent = parent.clone();
                self.folders.lock().unwrap().push(created);
                MailboxEditReceipt::resolved(MailboxId::try_from(id.as_str()).unwrap())
            }
            MailboxEdit::Update {
                target,
                name,
                parent,
            } => {
                self.place(target, name, parent.as_ref());
                MailboxEditReceipt::resolved(target.clone())
            }
            MailboxEdit::Trash {
                target,
                trash,
                name,
            } => {
                self.place(target, name, Some(trash));
                MailboxEditReceipt::resolved(target.clone())
            }
            MailboxEdit::Delete { target } => {
                self.folders.lock().unwrap().retain(|m| &m.id != target);
                MailboxEditReceipt::removed()
            }
        })
    }
}

fn work() -> MailboxId {
    MailboxId::try_from("work").unwrap()
}

fn rename_work(to: &str) -> MailboxChange {
    MailboxChange::Update {
        target: work(),
        seen: MailboxPlace {
            name: "Work".into(),
            parent: None,
        },
        name: to.into(),
        parent: None,
    }
}

async fn change(
    tree: &Tree,
    store: &SqliteStore<ManualClock>,
    idempotency: &str,
    change: &MailboxChange,
) -> Result<crate::MailboxEditOutcome, crate::SyncError> {
    edit_mailbox(
        tree,
        store,
        &account(),
        worker(),
        Duration::from_mins(1),
        idempotency,
        change,
    )
    .await
}

async fn drain(tree: &Tree, store: &SqliteStore<ManualClock>) -> crate::DrainReport {
    drain_outbox(tree, store, &account(), worker(), Duration::from_mins(1))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_new_folder_is_made_and_named_in_the_outcome() {
    let tree = Tree::new();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let create = MailboxChange::Create {
        name: "Receipts".into(),
        parent: Some(work()),
    };

    let outcome = change(&tree, &store, "create-receipts", &create)
        .await
        .unwrap();

    let made = outcome.mailbox.expect("a create names its folder");
    assert_eq!(tree.folder(made.as_str()).unwrap().parent, Some(work()));
    assert_eq!(
        store.pending_op_state(outcome.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

#[tokio::test]
async fn a_change_made_offline_goes_out_when_the_drain_finds_the_server() {
    // The read of the folder list is refused, so nothing reaches the server.
    let tree = Tree::new().offline_for(1);
    let (store, clock) = drain::store_and_clock();

    let err = change(&tree, &store, "rename-work", &rename_work("Projects"))
        .await
        .expect_err("offline");
    assert!(matches!(err, crate::SyncError::Provider(ref e) if e.is_retryable()));
    assert!(tree.sent().is_empty());
    assert_eq!(store.list_pending_ops(account()).await.unwrap().len(), 1);

    clock.advance(Duration::from_hours(1));
    let report = drain(&tree, &store).await;

    assert_eq!(report.attempted.len(), 1);
    assert_eq!(report.attempted[0].kind, PendingOpKind::MailboxEdit);
    assert_eq!(report.attempted[0].outcome, DrainOutcome::Succeeded);
    assert_eq!(report.attempted[0].provider_key, Some(work().key().clone()));
    assert_eq!(tree.folder("work").unwrap().name, "Projects");
}

#[tokio::test]
async fn a_queued_rename_does_not_overwrite_a_rename_made_elsewhere() {
    let tree = Tree::new().offline_for(1);
    let (store, clock) = drain::store_and_clock();
    change(&tree, &store, "rename-work", &rename_work("Projects"))
        .await
        .expect_err("offline");

    // While this device was offline, another client renamed the same folder.
    tree.rename_elsewhere("work", "Clients");
    clock.advance(Duration::from_hours(1));
    let report = drain(&tree, &store).await;

    assert_eq!(
        report.attempted[0].outcome,
        DrainOutcome::Failed {
            class: FailureClass::Conflict
        }
    );
    assert!(
        tree.sent().is_empty(),
        "nothing was sent over the other rename"
    );
    assert_eq!(tree.folder("work").unwrap().name, "Clients");
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_retried_create_does_not_make_a_second_folder() {
    let tree = Tree::new();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let create = MailboxChange::Create {
        name: "Receipts".into(),
        parent: None,
    };
    let first = change(&tree, &store, "create-1", &create).await.unwrap();
    // The same change under a new key: what a lost response and a user's second tap
    // both look like.
    let second = change(&tree, &store, "create-2", &create).await.unwrap();

    assert_eq!(first.mailbox, second.mailbox);
    assert_eq!(tree.sent().len(), 1);
}

#[tokio::test]
async fn a_trashed_folder_goes_into_trash_under_a_free_name() {
    let tree = Tree::new();
    tree.folders
        .lock()
        .unwrap()
        .push(mailbox("old-work", "Work", None));
    tree.place(
        &MailboxId::try_from("old-work").unwrap(),
        "Work",
        Some(&MailboxId::try_from("trash").unwrap()),
    );
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let trash = MailboxChange::Trash {
        target: work(),
        seen: MailboxPlace {
            name: "Work".into(),
            parent: None,
        },
        trash: MailboxId::try_from("trash").unwrap(),
    };

    change(&tree, &store, "trash-work", &trash).await.unwrap();

    let moved = tree.folder("work").unwrap();
    assert_eq!(moved.parent, Some(MailboxId::try_from("trash").unwrap()));
    assert_eq!(moved.name, "Work 2");
}

#[tokio::test]
async fn a_name_no_transport_accepts_is_refused_before_anything_is_queued() {
    let tree = Tree::new();
    let store = SqliteStore::open_in_memory(clock()).unwrap();

    let err = change(&tree, &store, "rename-bad", &rename_work("a/b"))
        .await
        .expect_err("refused");
    assert!(matches!(err, crate::SyncError::Provider(ref e)
        if e.class() == FailureClass::InvalidState));
    assert!(store.list_pending_ops(account()).await.unwrap().is_empty());
    assert!(tree.sent().is_empty());
}

#[tokio::test]
async fn deleting_a_folder_that_is_already_gone_succeeds_without_a_request() {
    let tree = Tree::new();
    let store = SqliteStore::open_in_memory(clock()).unwrap();
    let delete = MailboxChange::Delete {
        target: MailboxId::try_from("gone").unwrap(),
    };

    let outcome = change(&tree, &store, "delete-gone", &delete).await.unwrap();

    assert_eq!(outcome.mailbox, None);
    assert!(tree.sent().is_empty());
}
