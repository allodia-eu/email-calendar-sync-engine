//! Folder-tree edits over the fake executor: the `Mailbox/set` each edit sends, and how
//! Stalwart's refusals (captured in `mailbox_set_refusals_response.json`) are classified.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::{MailboxEdit, MailboxEditReceipt, MailboxWrites, Provider};
use serde_json::{Value, json};

use crate::provider::provider_test_support::{account, fixture, recording};

fn id(value: &str) -> MailboxId {
    MailboxId::try_from(value).unwrap()
}

/// A one-call response document answering call `0` with `arguments`.
fn answer(method: &str, arguments: &Value) -> Value {
    json!({ "methodResponses": [[method, arguments, "0"]], "sessionState": "s" })
}

/// The `n`th refusal Stalwart returned, re-addressed to call `0` and, for a create, to
/// this adapter's creation id.
fn observed(n: usize) -> Value {
    let doc = fixture("mailbox_set_refusals_response.json");
    let mut arguments = doc["methodResponses"][n][1].clone();
    if let Some(refused) = arguments
        .get_mut("notCreated")
        .and_then(|v| v.as_object_mut())
    {
        let error = refused.remove("d").expect("the captured creation id");
        refused.insert("new".to_owned(), error);
    }
    answer("Mailbox/set", &arguments)
}

/// Every method call the fake was sent, as `(method, arguments)`, across all requests.
fn calls(exec: &crate::provider::provider_test_support::FakeExecutor) -> Vec<(String, Value)> {
    exec.requests
        .lock()
        .unwrap()
        .iter()
        .flat_map(|r| r["methodCalls"].as_array().unwrap().clone())
        .map(|c| (c[0].as_str().unwrap().to_owned(), c[1].clone()))
        .collect()
}

async fn edit(
    responses: Vec<Value>,
    edit: &MailboxEdit,
) -> (
    Result<MailboxEditReceipt, FailureClass>,
    Vec<(String, Value)>,
) {
    let (provider, exec) = recording(responses);
    let result = provider
        .edit_mailbox(&account(), edit)
        .await
        .map_err(|e| e.class());
    (result, calls(&exec))
}

fn rename(target: &str, name: &str, parent: Option<&str>) -> MailboxEdit {
    MailboxEdit::Update {
        target: id(target),
        name: name.into(),
        parent: parent.map(id),
    }
}

#[test]
fn an_account_that_can_write_mail_can_change_its_folders() {
    let (provider, _exec) = recording(vec![]);
    assert!(provider.connection_info().capabilities.mailbox_writes());
}

#[tokio::test]
async fn a_create_sends_the_name_the_parent_and_a_subscription() {
    let created = answer(
        "Mailbox/set",
        &json!({ "created": { "new": { "id": "h" } } }),
    );
    let create = MailboxEdit::Create {
        name: "Reçus".into(),
        parent: Some(id("work")),
    };

    let (result, sent) = edit(vec![created], &create).await;

    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("h"))));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "Mailbox/set");
    assert_eq!(
        sent[0].1,
        json!({
            "accountId": "c",
            "create": { "new": { "name": "Reçus", "parentId": "work", "isSubscribed": true } }
        })
    );
}

#[tokio::test]
async fn a_create_at_the_top_sends_a_null_parent() {
    let created = answer(
        "Mailbox/set",
        &json!({ "created": { "new": { "id": "h" } } }),
    );
    let create = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: None,
    };
    let (_, sent) = edit(vec![created], &create).await;
    assert_eq!(sent[0].1["create"]["new"]["parentId"], Value::Null);
}

#[tokio::test]
async fn a_create_that_finds_its_folder_there_resolves_to_it() {
    let create = MailboxEdit::Create {
        name: "probe-parent".into(),
        parent: None,
    };
    let (result, sent) = edit(vec![observed(0)], &create).await;
    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("h"))));
    assert_eq!(
        sent.len(),
        1,
        "Stalwart names the existing folder, so no lookup"
    );
}

#[tokio::test]
async fn an_already_exists_without_the_id_is_looked_up_by_name() {
    let refused = answer(
        "Mailbox/set",
        &json!({ "notCreated": { "new": { "type": "alreadyExists" } } }),
    );
    let listed = answer(
        "Mailbox/get",
        &json!({ "list": [
            { "id": "x", "name": "Receipts", "parentId": "other" },
            { "id": "y", "name": "Receipts", "parentId": "work" }
        ] }),
    );
    let create = MailboxEdit::Create {
        name: "Receipts".into(),
        parent: Some(id("work")),
    };

    let (result, sent) = edit(vec![refused, listed], &create).await;

    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("y"))));
    assert_eq!(sent[1].0, "Mailbox/get");
}

#[tokio::test]
async fn a_rename_or_move_sets_the_name_and_the_parent() {
    let updated = answer("Mailbox/set", &json!({ "updated": { "w2024": null } }));

    let (result, sent) = edit(vec![updated], &rename("w2024", "2025", None)).await;

    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("w2024"))));
    assert_eq!(
        sent[0].1,
        json!({
            "accountId": "c",
            "update": { "w2024": { "name": "2025", "parentId": null } }
        })
    );
}

#[tokio::test]
async fn trashing_moves_the_folder_under_trash_by_its_new_name() {
    let updated = answer("Mailbox/set", &json!({ "updated": { "bills": null } }));
    let trash = MailboxEdit::Trash {
        target: id("bills"),
        trash: id("b"),
        name: "Bills 2".into(),
    };

    let (result, sent) = edit(vec![updated], &trash).await;

    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("bills"))));
    assert_eq!(
        sent[0].1["update"]["bills"],
        json!({ "name": "Bills 2", "parentId": "b" })
    );
}

#[tokio::test]
async fn a_tree_that_moved_under_the_change_is_a_conflict() {
    // A sibling holds the name, the parent is gone, the folder would sit inside itself,
    // and the target is gone: Stalwart's four answers, as captured.
    for (n, target) in [(1, "i"), (2, "i"), (3, "h"), (4, "zzz")] {
        let (result, _) = edit(vec![observed(n)], &rename(target, "x", Some("p"))).await;
        assert_eq!(result, Err(FailureClass::Conflict), "refusal {n}");
    }
}

#[tokio::test]
async fn a_name_the_server_refuses_is_invalid_state() {
    let refused = answer(
        "Mailbox/set",
        &json!({ "notUpdated": { "i": { "type": "invalidProperties", "properties": ["name"] } } }),
    );
    let (result, _) = edit(vec![refused], &rename("i", "bad", None)).await;
    assert_eq!(result, Err(FailureClass::InvalidState));
}

#[tokio::test]
async fn a_silently_dropped_update_is_never_a_success() {
    let dropped = answer("Mailbox/set", &json!({ "updated": {} }));
    let (result, _) = edit(vec![dropped], &rename("i", "x", None)).await;
    assert_eq!(result, Err(FailureClass::Conflict));
}

/// `work` holding `w2024` holding `q1`, plus `other` beside them.
fn tree() -> Value {
    answer(
        "Mailbox/get",
        &json!({ "list": [
            { "id": "work", "parentId": null },
            { "id": "w2024", "parentId": "work" },
            { "id": "q1", "parentId": "w2024" },
            { "id": "w2023", "parentId": "work" },
            { "id": "other", "parentId": null }
        ] }),
    )
}

#[tokio::test]
async fn a_delete_destroys_the_subtree_deepest_first_with_its_mail() {
    let destroyed = answer(
        "Mailbox/set",
        &json!({ "destroyed": ["w2023", "q1", "w2024", "work"] }),
    );

    let (result, sent) = edit(
        vec![tree(), destroyed],
        &MailboxEdit::Delete { target: id("work") },
    )
    .await;

    assert_eq!(result, Ok(MailboxEditReceipt::removed()));
    assert_eq!(sent[0].0, "Mailbox/get");
    assert_eq!(
        sent[1].1,
        json!({
            "accountId": "c",
            "destroy": ["w2023", "q1", "w2024", "work"],
            "onDestroyRemoveEmails": true
        })
    );
}

#[tokio::test]
async fn deleting_a_folder_that_is_gone_sends_nothing_more() {
    let (result, sent) = edit(vec![tree()], &MailboxEdit::Delete { target: id("gone") }).await;
    assert_eq!(result, Ok(MailboxEditReceipt::removed()));
    assert_eq!(sent.len(), 1);
}

#[tokio::test]
async fn a_subfolder_already_gone_does_not_fail_the_delete() {
    let partly = answer(
        "Mailbox/set",
        &json!({
            "destroyed": ["w2023", "w2024", "work"],
            "notDestroyed": { "q1": { "type": "notFound" } }
        }),
    );
    let (result, _) = edit(
        vec![tree(), partly],
        &MailboxEdit::Delete { target: id("work") },
    )
    .await;
    assert_eq!(result, Ok(MailboxEditReceipt::removed()));
}

#[tokio::test]
async fn a_delete_the_server_refuses_keeps_the_refusal() {
    // A folder that gained a child between the read and the destroy, as captured.
    let refusal = observed(5);
    let listed = answer(
        "Mailbox/get",
        &json!({ "list": [ { "id": "h", "parentId": null } ] }),
    );
    let (result, _) = edit(
        vec![listed, refusal],
        &MailboxEdit::Delete { target: id("h") },
    )
    .await;
    assert_eq!(result, Err(FailureClass::Permanent));
}
