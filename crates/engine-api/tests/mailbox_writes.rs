//! The facade's folder changes: a change reaches the server, and the store holds the result
//! when the call returns.

use std::sync::Mutex;

use async_trait::async_trait;
use engine_api::{
    AccountId, ApiError, CalendarWrites, Capabilities, Engine, MailboxChange, MailboxEdit,
    MailboxEditReceipt, MailboxPlace, MailboxWrites, Provider,
};
use engine_core::{
    ids::MailboxId,
    mail::{Mailbox, MailboxRole},
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};
use engine_provider::{ConnectionInfo, ProviderResult, ScopeSync};

struct Tree(Mutex<Vec<Mailbox>>);

impl Tree {
    fn new() -> Self {
        let mut inbox = Mailbox::new(id("inbox"), "Inbox");
        inbox.role = Some(MailboxRole::Inbox);
        Self(Mutex::new(vec![inbox, Mailbox::new(id("work"), "Work")]))
    }
}

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

fn account() -> AccountId {
    AccountId::try_from("acct-folders").unwrap()
}

#[async_trait]
impl Provider for Tree {
    fn connection_info(&self) -> ConnectionInfo {
        ConnectionInfo::new(Capabilities::none().with_mail().with_mailbox_writes())
    }

    fn mailbox_scope(&self, account: &AccountId) -> SyncScope {
        SyncScope::JmapType {
            account: account.clone(),
            data_type: JmapDataType::Mailbox,
        }
    }

    async fn sync_mailboxes(
        &self,
        _account: &AccountId,
        _cursor: Option<&SyncState>,
    ) -> ProviderResult<ScopeSync<Mailbox>> {
        let folders = self.0.lock().unwrap().clone();
        let present = folders.iter().map(|m| m.id.key().clone()).collect();
        Ok(ScopeSync::new(
            SyncUpdate::snapshot(folders, present),
            SyncState::new("tree"),
        ))
    }
}

impl CalendarWrites for Tree {}

#[async_trait]
impl MailboxWrites for Tree {
    async fn edit_mailbox(
        &self,
        _account: &AccountId,
        edit: &MailboxEdit,
    ) -> ProviderResult<MailboxEditReceipt> {
        let MailboxEdit::Create { name, parent } = edit else {
            panic!("only creates are asked of this fake");
        };
        let mut made = Mailbox::new(id("made"), name.clone());
        made.parent.clone_from(parent);
        self.0.lock().unwrap().push(made);
        Ok(MailboxEditReceipt::resolved(id("made")))
    }
}

#[tokio::test]
async fn a_new_folder_is_in_the_store_when_the_call_returns() {
    let engine = Engine::open_in_memory().unwrap();
    let tree = Tree::new();
    let create = MailboxChange::Create {
        name: "Receipts".into(),
        parent: Some(id("work")),
    };

    let write = engine
        .edit_mailbox(&tree, &account(), "create-receipts", &create)
        .await
        .unwrap();

    assert_eq!(write.outcome.mailbox, Some(id("made")));
    assert!(write.reconciled.is_ok());
    let stored = engine.mailboxes(&account()).await.unwrap();
    let made = stored.iter().find(|m| m.id == id("made")).expect("stored");
    assert_eq!(made.name, "Receipts");
    assert_eq!(made.parent, Some(id("work")));
}

#[tokio::test]
async fn a_name_no_transport_accepts_is_invalid_input() {
    let engine = Engine::open_in_memory().unwrap();
    let rename = MailboxChange::Update {
        target: id("work"),
        seen: MailboxPlace {
            name: "Work".into(),
            parent: None,
        },
        name: "Work/2024".into(),
        parent: None,
    };

    let err = engine
        .edit_mailbox(&Tree::new(), &account(), "rename-bad", &rename)
        .await
        .expect_err("refused");

    assert!(matches!(err, ApiError::InvalidInput(_)), "{err:?}");
    assert!(engine.outbox(&account()).await.unwrap().is_empty());
}
