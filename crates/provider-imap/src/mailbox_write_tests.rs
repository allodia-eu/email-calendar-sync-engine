//! Offline tests for folder writes, asserting the exact commands each edit sends.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{MailboxEdit, MailboxEditReceipt};

use super::edit_mailbox;
use crate::{
    mock::{MockStream, Recorded, script, written},
    transport::Connection,
};

const GREETING: &str = "* OK ready\r\n";
const LOGIN_OK: &str = "a1 OK LOGIN ok\r\n";

/// `a2`'s `LIST`: INBOX, a Trash, and `Work` holding `2024`, which holds `Q1`.
const TREE: &str = concat!(
    "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n",
    "* LIST (\\HasNoChildren \\Trash) \"/\" \"Trash\"\r\n",
    "* LIST (\\HasChildren) \"/\" \"Work\"\r\n",
    "* LIST (\\HasChildren) \"/\" \"Work/2024\"\r\n",
    "* LIST (\\HasNoChildren) \"/\" \"Work/2024/Q1\"\r\n",
    "a2 OK LIST done\r\n",
);

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

async fn logged_in(server: Vec<u8>) -> (Connection<MockStream>, Recorded) {
    let (stream, recorded) = MockStream::new(server);
    let mut conn = Connection::open(stream).await.unwrap();
    conn.login("alice", "pw").await.unwrap();
    (conn, recorded)
}

/// Runs `edit` against a server answering `replies` after greeting, login and `list`.
async fn run(
    list: &str,
    replies: &[&str],
    edit: &MailboxEdit,
) -> (engine_provider::ProviderResult<MailboxEditReceipt>, String) {
    let mut parts = vec![GREETING, LOGIN_OK, list];
    parts.extend_from_slice(replies);
    let (mut conn, recorded) = logged_in(script(&parts)).await;
    let result = edit_mailbox(&mut conn, edit).await;
    (result, written(&recorded))
}

#[tokio::test]
async fn a_new_folder_is_created_under_its_parent_in_the_wire_encoding_and_subscribed() {
    let create = MailboxEdit::Create {
        name: "Reçus".into(),
        parent: Some(id("Work")),
    };
    let (result, sent) = run(TREE, &["a3 OK CREATE\r\n", "a4 OK SUBSCRIBE\r\n"], &create).await;

    assert_eq!(
        result.unwrap(),
        MailboxEditReceipt::resolved(id("Work/Reçus"))
    );
    assert!(sent.contains("a2 LIST \"\" \"*\""), "{sent}");
    assert!(sent.contains("a3 CREATE \"Work/Re&AOc-us\""), "{sent}");
    assert!(sent.contains("a4 SUBSCRIBE \"Work/Re&AOc-us\""), "{sent}");
}

#[tokio::test]
async fn a_create_the_server_says_already_exists_is_a_success() {
    let create = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: None,
    };
    let (result, sent) = run(
        TREE,
        &[
            "a3 NO [ALREADYEXISTS] Mailbox exists\r\n",
            "a4 OK SUBSCRIBE\r\n",
        ],
        &create,
    )
    .await;

    assert_eq!(
        result.unwrap(),
        MailboxEditReceipt::resolved(id("Receipts"))
    );
    assert!(sent.contains("a3 CREATE \"Receipts\""), "{sent}");
}

#[tokio::test]
async fn a_folder_the_list_already_holds_is_not_created_again() {
    let create = MailboxEdit::Create {
        name: "2024".into(),
        parent: Some(id("Work")),
    };
    let (result, sent) = run(TREE, &["a3 OK SUBSCRIBE\r\n"], &create).await;

    assert_eq!(
        result.unwrap(),
        MailboxEditReceipt::resolved(id("Work/2024"))
    );
    assert!(!sent.contains("CREATE"), "{sent}");
}

#[tokio::test]
async fn a_name_carrying_the_delimiter_is_refused_before_anything_is_sent() {
    let create = MailboxEdit::Create {
        name: "a/b".into(),
        parent: None,
    };
    let (result, sent) = run(TREE, &[], &create).await;

    assert_eq!(result.unwrap_err().class(), FailureClass::InvalidState);
    assert!(!sent.contains("CREATE"), "{sent}");
}

#[tokio::test]
async fn a_server_with_no_hierarchy_keeps_no_subfolders() {
    let flat = "* LIST () NIL \"INBOX\"\r\n* LIST () NIL \"Work\"\r\na2 OK LIST done\r\n";
    let create = MailboxEdit::Create {
        name: "Inner".into(),
        parent: Some(id("Work")),
    };
    let (result, sent) = run(flat, &[], &create).await;

    assert_eq!(result.unwrap_err().class(), FailureClass::InvalidState);
    assert!(!sent.contains("CREATE"), "{sent}");
}

#[tokio::test]
async fn a_move_renames_the_path_and_moves_every_subscription_beneath_it() {
    let update = MailboxEdit::Update {
        target: id("Work/2024"),
        name: "2025".into(),
        parent: None,
    };
    let replies = [
        "a3 OK RENAME\r\n",
        "a4 OK\r\n",
        "a5 OK\r\n",
        "a6 OK\r\n",
        "a7 OK\r\n",
    ];
    let (result, sent) = run(TREE, &replies, &update).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::resolved(id("2025")));
    assert!(sent.contains("a3 RENAME \"Work/2024\" \"2025\""), "{sent}");
    assert!(sent.contains("a4 UNSUBSCRIBE \"Work/2024/Q1\""), "{sent}");
    assert!(sent.contains("a5 SUBSCRIBE \"2025/Q1\""), "{sent}");
    assert!(sent.contains("a6 UNSUBSCRIBE \"Work/2024\""), "{sent}");
    assert!(sent.contains("a7 SUBSCRIBE \"2025\""), "{sent}");
}

#[tokio::test]
async fn a_rename_the_server_finds_nothing_to_rename_is_a_conflict() {
    let update = MailboxEdit::Update {
        target: id("Work"),
        name: "Clients".into(),
        parent: None,
    };
    let (result, _) = run(TREE, &["a3 NO [NONEXISTENT] No such mailbox\r\n"], &update).await;

    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_rename_onto_a_folder_that_exists_is_a_conflict_without_a_request() {
    let update = MailboxEdit::Update {
        target: id("Work/2024/Q1"),
        name: "2024".into(),
        parent: Some(id("Work")),
    };
    let (result, sent) = run(TREE, &[], &update).await;

    assert_eq!(result.unwrap_err().class(), FailureClass::Conflict);
    assert!(!sent.contains("RENAME"), "{sent}");
}

#[tokio::test]
async fn a_trashed_folder_is_renamed_into_trash() {
    let trash = MailboxEdit::Trash {
        target: id("Work"),
        trash: id("Trash"),
        name: "Work".into(),
    };
    let replies = [
        "a3 OK RENAME\r\n",
        "a4 OK\r\n",
        "a5 OK\r\n",
        "a6 OK\r\n",
        "a7 OK\r\n",
        "a8 OK\r\n",
        "a9 OK\r\n",
    ];
    let (result, sent) = run(TREE, &replies, &trash).await;

    assert_eq!(
        result.unwrap(),
        MailboxEditReceipt::resolved(id("Trash/Work"))
    );
    assert!(sent.contains("a3 RENAME \"Work\" \"Trash/Work\""), "{sent}");
    assert!(sent.contains("SUBSCRIBE \"Trash/Work/2024/Q1\""), "{sent}");
}

#[tokio::test]
async fn a_trash_that_cannot_hold_folders_is_filled_with_their_mail_instead() {
    let list = concat!(
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n",
        "* LIST (\\Noinferiors \\Trash) \"/\" \"Trash\"\r\n",
        "* LIST (\\HasChildren) \"/\" \"Work\"\r\n",
        "* LIST (\\HasNoChildren) \"/\" \"Work/A\"\r\n",
        "a2 OK LIST done\r\n",
    );
    let trash = MailboxEdit::Trash {
        target: id("Work"),
        trash: id("Trash"),
        name: "Work".into(),
    };
    let replies = [
        "* 2 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na3 OK [READ-WRITE] done\r\n",
        "a4 OK MOVE done\r\n",
        "* 0 EXISTS\r\n* OK [UIDVALIDITY 8] v\r\na5 OK [READ-WRITE] done\r\n",
        "* 0 EXISTS\r\n* OK [UIDVALIDITY 1] v\r\na6 OK [READ-ONLY] done\r\n",
        "a7 OK DELETE\r\n",
        "a8 OK\r\n",
        "a9 OK DELETE\r\n",
        "a10 OK\r\n",
    ];
    let (result, sent) = run(list, &replies, &trash).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::removed());
    assert!(!sent.contains("RENAME"), "{sent}");
    assert!(sent.contains("a3 SELECT \"Work/A\""), "{sent}");
    assert!(sent.contains("a4 UID MOVE 1:* \"Trash\""), "{sent}");
    assert!(sent.contains("a5 SELECT \"Work\""), "{sent}");
    assert!(
        !sent.contains("a6 UID MOVE"),
        "an empty folder moves nothing: {sent}"
    );
    assert!(sent.contains("a6 EXAMINE \"INBOX\""), "{sent}");
    assert!(sent.contains("a7 DELETE \"Work/A\""), "{sent}");
    assert!(sent.contains("a9 DELETE \"Work\""), "{sent}");
}

#[tokio::test]
async fn a_trash_that_refuses_the_rename_is_filled_with_the_mail_instead() {
    let list = concat!(
        "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n",
        "* LIST (\\HasNoChildren \\Trash) \"/\" \"Trash\"\r\n",
        "* LIST (\\HasNoChildren) \"/\" \"Work\"\r\n",
        "a2 OK LIST done\r\n",
    );
    let trash = MailboxEdit::Trash {
        target: id("Work"),
        trash: id("Trash"),
        name: "Work".into(),
    };
    let replies = [
        "a3 NO [CANNOT] Trash holds no folders\r\n",
        "* 1 EXISTS\r\n* OK [UIDVALIDITY 7] v\r\na4 OK [READ-WRITE] done\r\n",
        "a5 OK MOVE done\r\n",
        "* 0 EXISTS\r\n* OK [UIDVALIDITY 1] v\r\na6 OK [READ-ONLY] done\r\n",
        "a7 OK DELETE\r\n",
        "a8 OK\r\n",
    ];
    let (result, sent) = run(list, &replies, &trash).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::removed());
    assert!(sent.contains("a3 RENAME \"Work\" \"Trash/Work\""), "{sent}");
    assert!(sent.contains("a5 UID MOVE 1:* \"Trash\""), "{sent}");
    assert!(sent.contains("a7 DELETE \"Work\""), "{sent}");
}

#[tokio::test]
async fn a_delete_removes_the_subtree_deepest_first_and_tolerates_what_is_gone() {
    let delete = MailboxEdit::Delete {
        target: id("Work/2024"),
    };
    let replies = [
        "* 0 EXISTS\r\n* OK [UIDVALIDITY 1] v\r\na3 OK [READ-ONLY] done\r\n",
        "a4 NO [NONEXISTENT] already gone\r\n",
        "a5 OK\r\n",
        "a6 OK DELETE\r\n",
        "a7 OK\r\n",
    ];
    let (result, sent) = run(TREE, &replies, &delete).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::removed());
    assert!(sent.contains("a4 DELETE \"Work/2024/Q1\""), "{sent}");
    assert!(sent.contains("a6 DELETE \"Work/2024\""), "{sent}");
    assert!(
        !sent.contains("DELETE \"Work\"\r"),
        "the parent stays: {sent}"
    );
}

#[tokio::test]
async fn deleting_a_folder_the_list_does_not_hold_sends_nothing() {
    let delete = MailboxEdit::Delete { target: id("Gone") };
    let (result, sent) = run(TREE, &[], &delete).await;

    assert_eq!(result.unwrap(), MailboxEditReceipt::removed());
    assert!(!sent.contains("DELETE"), "{sent}");
}

#[tokio::test]
async fn the_inbox_is_refused_before_the_server_is_asked_anything() {
    let delete = MailboxEdit::Delete {
        target: id("INBOX"),
    };
    let (result, sent) = run(TREE, &[], &delete).await;

    assert_eq!(result.unwrap_err().class(), FailureClass::InvalidState);
    assert!(!sent.contains("LIST"), "{sent}");
}
