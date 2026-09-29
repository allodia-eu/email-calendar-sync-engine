//! Offline tests for folder writes, asserting what each edit sends.
//!
//! ⚠️ The response bodies here are **documented, not observed**: they follow the
//! `mailFolder` shapes and the `{ "error": { "code", "message" } }` envelope on
//! learn.microsoft.com (`mailfolder-post-childfolders`, `mailfolder-update`,
//! `mailfolder-move`, `mailfolder-permanentdelete`), and `ErrorFolderExists` is the code
//! Exchange is known to use for a duplicate name, not one captured here. The gated
//! `tests/live_mailbox_writes.rs` is what replaces them with the real bytes.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{MailboxEdit, MailboxEditReceipt, Provider};
use serde_json::{Value, json};

use super::edit_mailbox;
use crate::{
    GraphProvider,
    test_support::{FakeRoute, RequestLog, fake_client, fake_client_logged},
    transport::GraphClient,
};

const BASE: &str = "https://graph.test/me";

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

fn folder(id: &str, name: &str, parent: &str) -> Value {
    json!({
        "id": id,
        "displayName": name,
        "parentFolderId": parent,
        "childFolderCount": 0,
        "unreadItemCount": 0,
        "totalItemCount": 0,
        "isHidden": false
    })
}

fn graph_error(status: u16, code: &str) -> FakeRoute {
    Err((
        status,
        json!({ "error": { "code": code, "message": "documented shape" } }),
    ))
}

fn logged(routes: Vec<(&str, FakeRoute)>) -> (GraphClient, RequestLog) {
    let (client, _, requests) = fake_client_logged(routes);
    (client, requests)
}

/// The `(method, path, body)` of every request, with the base stripped.
fn sent(log: &RequestLog) -> Vec<(&'static str, String, Option<Value>)> {
    log.lock()
        .unwrap()
        .iter()
        .map(|(method, url, body)| {
            (
                *method,
                url.strip_prefix(BASE).unwrap_or(url).to_owned(),
                body.clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_top_level_folder_is_made_under_the_mailbox_root() {
    let (client, log) = logged(vec![(
        "/childFolders",
        Ok(folder("folder-new", "Receipts", "folder-root")),
    )]);
    let edit = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: None,
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt, MailboxEditReceipt::resolved(id("folder-new")));
    assert_eq!(
        sent(&log),
        vec![(
            "POST",
            "/mailFolders/msgfolderroot/childFolders".to_owned(),
            Some(json!({ "displayName": "Receipts" })),
        )]
    );
}

#[tokio::test]
async fn a_nested_folder_is_made_under_its_parent() {
    let (client, log) = logged(vec![(
        "/childFolders",
        Ok(folder("folder-new", "2025", "folder-work")),
    )]);
    let edit = MailboxEdit::Create {
        name: "2025".into(),
        parent: Some(id("folder-work")),
    };

    edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(sent(&log)[0].1, "/mailFolders/folder-work/childFolders");
}

#[tokio::test]
async fn a_create_that_finds_its_folder_already_made_succeeds_with_it() {
    let (client, log) = logged(vec![
        (
            "/childFolders?$top",
            Ok(json!({ "value": [
                folder("folder-other", "Bills", "folder-root"),
                folder("folder-existing", "Receipts", "folder-root"),
            ] })),
        ),
        ("/childFolders", graph_error(409, "ErrorFolderExists")),
    ]);
    let edit = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: None,
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("folder-existing")));
    let requests = sent(&log);
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].0, "GET");
    assert!(
        requests[1]
            .1
            .starts_with("/mailFolders/msgfolderroot/childFolders?")
    );
}

#[tokio::test]
async fn a_name_taken_only_by_a_differently_cased_folder_is_a_conflict() {
    let (client, _) = logged(vec![
        (
            "/childFolders?$top",
            Ok(json!({ "value": [folder("folder-lower", "receipts", "folder-root")] })),
        ),
        ("/childFolders", graph_error(409, "ErrorFolderExists")),
    ]);
    let edit = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: None,
    };

    let err = edit_mailbox(&client, &edit).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
}

/// Routes for an update of `folder-a`, currently called `Old` at the top level.
fn update_routes(write: (&'static str, FakeRoute)) -> Vec<(&'static str, FakeRoute)> {
    vec![
        (
            "/mailFolders/folder-a?$select",
            Ok(folder("folder-a", "Old", "folder-root")),
        ),
        (
            "/mailFolders/msgfolderroot",
            Ok(json!({ "id": "folder-root" })),
        ),
        write,
    ]
}

#[tokio::test]
async fn a_rename_patches_the_name_and_moves_nothing() {
    let (client, log) = logged(update_routes((
        "/mailFolders/folder-a",
        Ok(folder("folder-a", "New", "folder-root")),
    )));
    let edit = MailboxEdit::Update {
        target: id("folder-a"),
        name: "New".into(),
        parent: None,
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("folder-a")));
    let writes: Vec<_> = sent(&log).into_iter().filter(|r| r.0 != "GET").collect();
    assert_eq!(
        writes,
        vec![(
            "PATCH",
            "/mailFolders/folder-a".to_owned(),
            Some(json!({ "displayName": "New" })),
        )]
    );
}

#[tokio::test]
async fn a_move_posts_the_destination_and_renames_nothing() {
    let (client, log) = logged(update_routes((
        "/mailFolders/folder-a/move",
        Ok(folder("folder-a", "Old", "folder-b")),
    )));
    let edit = MailboxEdit::Update {
        target: id("folder-a"),
        name: "Old".into(),
        parent: Some(id("folder-b")),
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("folder-a")));
    let writes: Vec<_> = sent(&log).into_iter().filter(|r| r.0 != "GET").collect();
    assert_eq!(
        writes,
        vec![(
            "POST",
            "/mailFolders/folder-a/move".to_owned(),
            Some(json!({ "destinationId": "folder-b" })),
        )]
    );
}

#[tokio::test]
async fn a_folder_that_is_gone_is_a_conflict_and_nothing_is_written() {
    let (client, log) = logged(vec![(
        "/mailFolders/folder-a?$select",
        graph_error(404, "ErrorItemNotFound"),
    )]);
    let edit = MailboxEdit::Update {
        target: id("folder-a"),
        name: "New".into(),
        parent: Some(id("folder-b")),
    };

    let err = edit_mailbox(&client, &edit).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
    assert!(sent(&log).iter().all(|r| r.0 == "GET"));
}

#[tokio::test]
async fn a_rename_onto_a_taken_name_is_a_conflict() {
    let (client, _) = logged(update_routes((
        "/mailFolders/folder-a",
        graph_error(409, "ErrorFolderExists"),
    )));
    let edit = MailboxEdit::Update {
        target: id("folder-a"),
        name: "Taken".into(),
        parent: None,
    };

    let err = edit_mailbox(&client, &edit).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Conflict);
}

#[tokio::test]
async fn a_trashed_folder_is_renamed_before_it_moves_into_trash() {
    let (client, log) = logged(vec![
        (
            "/mailFolders/folder-a?$select",
            Ok(folder("folder-a", "Bills", "folder-root")),
        ),
        (
            "/mailFolders/folder-a/move",
            Ok(folder("folder-a", "Bills 2", "folder-deleteditems")),
        ),
        (
            "/mailFolders/folder-a",
            Ok(folder("folder-a", "Bills 2", "folder-root")),
        ),
    ]);
    let edit = MailboxEdit::Trash {
        target: id("folder-a"),
        trash: id("folder-deleteditems"),
        name: "Bills 2".into(),
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt.mailbox, Some(id("folder-a")));
    let writes: Vec<_> = sent(&log).into_iter().filter(|r| r.0 != "GET").collect();
    assert_eq!(
        writes,
        vec![
            (
                "PATCH",
                "/mailFolders/folder-a".to_owned(),
                Some(json!({ "displayName": "Bills 2" })),
            ),
            (
                "POST",
                "/mailFolders/folder-a/move".to_owned(),
                Some(json!({ "destinationId": "folder-deleteditems" })),
            ),
        ]
    );
}

#[tokio::test]
async fn a_delete_is_a_permanent_delete_and_a_folder_already_gone_is_done() {
    let (client, log) = logged(vec![("/permanentDelete", Ok(Value::Null))]);
    let edit = MailboxEdit::Delete {
        target: id("folder-a"),
    };

    let receipt = edit_mailbox(&client, &edit).await.unwrap();

    assert_eq!(receipt, MailboxEditReceipt::removed());
    assert_eq!(
        sent(&log),
        vec![(
            "POST",
            "/mailFolders/folder-a/permanentDelete".to_owned(),
            None,
        )]
    );

    let (gone, _) = logged(vec![(
        "/permanentDelete",
        graph_error(404, "ErrorItemNotFound"),
    )]);
    assert_eq!(
        edit_mailbox(&gone, &edit).await.unwrap(),
        MailboxEditReceipt::removed()
    );
}

#[tokio::test]
async fn a_server_failure_on_delete_stays_retryable() {
    let (client, _) = logged(vec![(
        "/permanentDelete",
        graph_error(503, "ServiceUnavailable"),
    )]);
    let edit = MailboxEdit::Delete {
        target: id("folder-a"),
    };

    let err = edit_mailbox(&client, &edit).await.unwrap_err();

    assert_eq!(err.class(), FailureClass::Retryable);
}

#[test]
fn the_mail_provider_advertises_folder_writes() {
    let provider = GraphProvider::new(fake_client(vec![]), id("inbox"));
    assert!(provider.connection_info().capabilities.mailbox_writes());
}
