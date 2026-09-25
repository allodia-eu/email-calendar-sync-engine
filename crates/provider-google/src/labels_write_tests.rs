//! Offline folder-change tests: which label requests an edit sends, in which order, with which
//! bodies. The fake answers canned bytes, so every test asserts the logged request itself.
//!
//! The label list below follows the documented `users.labels.list` shape (id, full `/`-joined
//! name, `type`); it is written by hand, not captured.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{MailboxEdit, Provider};
use serde_json::json;

use super::edit;
use crate::{
    GmailProvider,
    test_support::{FakeRoute, Request, fake_client, json, recording_client},
};

const DRAFT_NOT_FOUND: &str = include_str!("../tests/fixtures/error/draft_not_found.json");

fn labels() -> serde_json::Value {
    json!({ "labels": [
        { "id": "INBOX", "name": "INBOX", "type": "system" },
        { "id": "TRASH", "name": "TRASH", "type": "system" },
        { "id": "UNREAD", "name": "UNREAD", "type": "system" },
        { "id": "Label_1", "name": "Work", "type": "user" },
        { "id": "Label_2", "name": "Work/Clients", "type": "user" },
        { "id": "Label_3", "name": "Work/Clients/Acme", "type": "user" },
        { "id": "Label_4", "name": "Home", "type": "user" },
        { "id": "Label_5", "name": "Workshop", "type": "user" },
    ]})
}

fn not_found() -> FakeRoute {
    Err((404, json(DRAFT_NOT_FOUND)))
}

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

fn writes(log: &[Request]) -> Vec<Request> {
    log.iter().filter(|r| r.method != "GET").cloned().collect()
}

fn patched(log: &[Request]) -> Vec<(String, String)> {
    writes(log)
        .into_iter()
        .map(|r| {
            let label = r.url.rsplit('/').next().unwrap().to_owned();
            (label, r.body.unwrap()["name"].as_str().unwrap().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn a_folder_is_created_under_its_parents_full_name() {
    let (client, log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        (
            "POST /users/me/labels",
            Ok(json!({ "id": "Label_9", "name": "Work/Receipts", "type": "user" })),
        ),
    ]);
    let create = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: Some(id("Label_1")),
    };

    let receipt = edit(&client, &create).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("Label_9")));
    let sent = writes(&log.lock().unwrap());
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert!(
        sent[0].url.ends_with("/gmail/v1/users/me/labels"),
        "{}",
        sent[0].url
    );
    assert_eq!(
        sent[0].body,
        Some(json!({
            "name": "Work/Receipts",
            "labelListVisibility": "labelShow",
            "messageListVisibility": "show",
        }))
    );
}

#[tokio::test]
async fn a_folder_that_already_exists_is_not_created_twice() {
    let (client, log) = recording_client(vec![("GET /users/me/labels", Ok(labels()))]);
    let create = MailboxEdit::Create {
        name: "Clients".into(),
        parent: Some(id("Label_1")),
    };

    let receipt = edit(&client, &create).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("Label_2")));
    assert!(writes(&log.lock().unwrap()).is_empty());
}

#[tokio::test]
async fn a_rename_carries_every_label_beneath_it_and_renames_the_folder_last() {
    let (client, log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        ("PATCH /users/me/labels/", Ok(json!({}))),
    ]);
    let rename = MailboxEdit::Update {
        target: id("Label_1"),
        name: "Projects".into(),
        parent: None,
    };

    let receipt = edit(&client, &rename).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("Label_1")));
    // `Workshop` shares the prefix `Work` but is not beneath it.
    assert_eq!(
        patched(&log.lock().unwrap()),
        vec![
            ("Label_2".into(), "Projects/Clients".into()),
            ("Label_3".into(), "Projects/Clients/Acme".into()),
            ("Label_1".into(), "Projects".into()),
        ]
    );
}

#[tokio::test]
async fn a_move_puts_the_folder_and_what_it_holds_under_the_new_parent() {
    let (client, log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        ("PATCH /users/me/labels/", Ok(json!({}))),
    ]);
    let move_under_home = MailboxEdit::Update {
        target: id("Label_2"),
        name: "Clients".into(),
        parent: Some(id("Label_4")),
    };

    edit(&client, &move_under_home).await.unwrap();

    assert_eq!(
        patched(&log.lock().unwrap()),
        vec![
            ("Label_3".into(), "Home/Clients/Acme".into()),
            ("Label_2".into(), "Home/Clients".into()),
        ]
    );
}

#[tokio::test]
async fn a_name_another_label_holds_is_a_conflict_before_anything_is_written() {
    let (client, log) = recording_client(vec![("GET /users/me/labels", Ok(labels()))]);
    let clash = MailboxEdit::Update {
        target: id("Label_4"),
        name: "Workshop".into(),
        parent: None,
    };

    let err = edit(&client, &clash).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(writes(&log.lock().unwrap()).is_empty());
}

#[tokio::test]
async fn a_system_label_is_refused_and_a_missing_one_is_a_conflict() {
    let client = fake_client(vec![("/users/me/labels", labels())]);
    let inbox = MailboxEdit::Update {
        target: id("INBOX"),
        name: "Old".into(),
        parent: None,
    };
    assert_eq!(
        edit(&client, &inbox).await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let under_trash = MailboxEdit::Create {
        name: "Kept".into(),
        parent: Some(id("TRASH")),
    };
    assert_eq!(
        edit(&client, &under_trash).await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let under_all_mail = MailboxEdit::Create {
        name: "Kept".into(),
        parent: Some(id("ALL_MAIL")),
    };
    assert_eq!(
        edit(&client, &under_all_mail).await.unwrap_err().class(),
        FailureClass::InvalidState
    );
    let gone = MailboxEdit::Update {
        target: id("Label_77"),
        name: "Back".into(),
        parent: None,
    };
    assert_eq!(
        edit(&client, &gone).await.unwrap_err().class(),
        FailureClass::Conflict
    );
}

#[tokio::test]
async fn a_label_that_goes_during_a_rename_is_a_conflict() {
    let (client, _log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        ("PATCH /users/me/labels/Label_4", not_found()),
    ]);
    let rename = MailboxEdit::Update {
        target: id("Label_4"),
        name: "House".into(),
        parent: None,
    };

    assert_eq!(
        edit(&client, &rename).await.unwrap_err().class(),
        FailureClass::Conflict
    );
}

#[tokio::test]
async fn trashing_sends_only_mail_filed_nowhere_else_to_trash_then_removes_the_labels() {
    let minimal = |labels: &[&str]| Ok(json!({ "id": "x", "labelIds": labels }));
    let (client, log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        (
            "GET labelIds=Label_2&maxResults=500&pageToken=p2",
            Ok(json!({ "messages": [{ "id": "m3", "threadId": "t3" }] })),
        ),
        (
            "GET labelIds=Label_2",
            Ok(json!({
                "messages": [{ "id": "m1", "threadId": "t1" }, { "id": "m2", "threadId": "t2" }],
                "nextPageToken": "p2",
            })),
        ),
        (
            "GET labelIds=Label_3",
            Ok(json!({ "messages": [{ "id": "m4", "threadId": "t4" }] })),
        ),
        // Only this subtree's label: goes to Trash.
        ("GET /messages/m1?", minimal(&["Label_2", "UNREAD"])),
        // Also in the Inbox: stays, and only loses the label.
        ("GET /messages/m2?", minimal(&["Label_2", "INBOX"])),
        // Also filed under `Home`, outside the subtree: stays.
        ("GET /messages/m3?", minimal(&["Label_3", "Label_4"])),
        // Importance and a category are not places: goes to Trash.
        (
            "GET /messages/m4?",
            minimal(&["Label_3", "IMPORTANT", "CATEGORY_UPDATES"]),
        ),
        ("POST /messages/batchModify", Ok(serde_json::Value::Null)),
        ("DELETE /users/me/labels/", Ok(serde_json::Value::Null)),
    ]);
    let trash = MailboxEdit::Trash {
        target: id("Label_2"),
        trash: id("TRASH"),
        name: "Clients".into(),
    };

    let receipt = edit(&client, &trash).await.unwrap();

    assert_eq!(receipt.mailbox, None);
    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|r| r.url.contains("/messages/m3?")),
        "the second page of the list was read"
    );
    let sent = writes(&log);
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[0].method, "POST");
    assert!(sent[0].url.ends_with("/messages/batchModify"));
    let body = sent[0].body.clone().unwrap();
    let mut ids: Vec<&str> = body["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["m1", "m4"]);
    assert_eq!(body["addLabelIds"], json!(["TRASH"]));
    // The deepest label goes first.
    assert_eq!(sent[1].method, "DELETE");
    assert!(sent[1].url.ends_with("/labels/Label_3"));
    assert!(sent[2].url.ends_with("/labels/Label_2"));
}

#[tokio::test]
async fn a_delete_removes_the_labels_beneath_first_and_a_label_already_gone_is_removed() {
    let (client, log) = recording_client(vec![
        ("GET /users/me/labels", Ok(labels())),
        ("DELETE /users/me/labels/Label_2", not_found()),
        ("DELETE /users/me/labels/", Ok(serde_json::Value::Null)),
    ]);

    let receipt = edit(
        &client,
        &MailboxEdit::Delete {
            target: id("Label_1"),
        },
    )
    .await
    .unwrap();

    assert_eq!(receipt.mailbox, None);
    let deleted: Vec<String> = writes(&log.lock().unwrap())
        .into_iter()
        .map(|r| r.url.rsplit('/').next().unwrap().to_owned())
        .collect();
    assert_eq!(deleted, vec!["Label_3", "Label_2", "Label_1"]);
}

#[tokio::test]
async fn deleting_a_label_that_is_gone_sends_nothing() {
    let (client, log) = recording_client(vec![("GET /users/me/labels", Ok(labels()))]);

    let receipt = edit(
        &client,
        &MailboxEdit::Delete {
            target: id("Label_77"),
        },
    )
    .await
    .unwrap();

    assert_eq!(receipt.mailbox, None);
    assert!(writes(&log.lock().unwrap()).is_empty());
}

#[test]
fn gmail_advertises_folder_changes() {
    let provider = GmailProvider::new(fake_client(vec![]));
    assert!(provider.connection_info().capabilities.mailbox_writes());
}
