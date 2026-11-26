//! A create that asks for a role: the `role` it sends, the roles refused before sending, and
//! how a refusal is told apart: the folder an earlier attempt made, a role another folder
//! holds, a role the server will not give.

use engine_core::{error::FailureClass, mail::MailboxRole};
use engine_provider::{
    Capabilities, CreateRoleError, MailboxEdit, MailboxEditReceipt, MailboxWrites, Provider,
    ProviderError,
};
use serde_json::{Value, json};

use super::{answer, edit, id};
use crate::provider::provider_test_support::{fixture, recording};

fn created() -> Value {
    answer(
        "Mailbox/set",
        &json!({ "created": { "new": { "id": "h" } } }),
    )
}

/// The refusal of the one create, as `{ "type": kind, "properties": properties }`.
fn not_created(kind: &str, properties: &[&str]) -> Value {
    answer(
        "Mailbox/set",
        &json!({ "notCreated": { "new": { "type": kind, "properties": properties } } }),
    )
}

/// `Mailbox/get` listing `mailboxes`, each `(id, name, parentId, role)`.
fn listed(mailboxes: &[(&str, &str, Option<&str>, Option<&str>)]) -> Value {
    let list: Vec<Value> = mailboxes
        .iter()
        .map(|(id, name, parent, role)| {
            json!({ "id": id, "name": name, "parentId": parent, "role": role })
        })
        .collect();
    answer("Mailbox/get", &json!({ "list": list }))
}

fn rule(error: &ProviderError) -> Option<CreateRoleError> {
    std::error::Error::source(error)
        .and_then(|s| s.downcast_ref::<CreateRoleError>())
        .cloned()
}

fn archive() -> MailboxEdit {
    MailboxEdit::create_with_role("Archive", None, MailboxRole::Archive)
}

#[test]
fn a_jmap_account_that_can_change_its_folders_can_give_one_a_role() {
    let (provider, _exec) = recording(vec![]);
    assert!(
        provider
            .connection_info()
            .capabilities
            .mailbox_create_role()
    );
}

/// `Mailbox/get` of one mailbox's `role`, as the read-back after a create asks it.
fn role_of(id: &str, role: Option<&str>) -> Value {
    answer(
        "Mailbox/get",
        &json!({ "list": [ { "id": id, "role": role } ], "notFound": [] }),
    )
}

#[tokio::test]
async fn a_create_with_a_role_sends_the_registered_jmap_role_and_reads_it_back() {
    let (result, sent) = edit(vec![created(), role_of("h", Some("archive"))], &archive()).await;

    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("h"))));
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].1,
        json!({
            "accountId": "c",
            "create": { "new": {
                "name": "Archive", "parentId": null, "isSubscribed": true, "role": "archive"
            } }
        })
    );
    // Success is read, not assumed: the server is to echo only what it changed (RFC 8620 §5.3).
    assert_eq!(sent[1].0, "Mailbox/get");
    assert_eq!(
        sent[1].1,
        json!({ "accountId": "c", "ids": ["h"], "properties": ["role"] })
    );
}

#[tokio::test]
async fn a_role_the_server_echoes_needs_no_read_back() {
    let echoed = answer(
        "Mailbox/set",
        &json!({ "created": { "new": { "id": "h", "role": "archive" } } }),
    );
    let (result, sent) = edit(vec![echoed], &archive()).await;
    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("h"))));
    assert_eq!(sent.len(), 1);
}

#[tokio::test]
async fn a_create_the_server_took_without_the_role_is_not_a_success() {
    for read_back in [
        role_of("h", None),
        role_of("h", Some("junk")),
        answer("Mailbox/get", &json!({ "list": [], "notFound": ["h"] })),
    ] {
        let (provider, _exec) = recording(vec![created(), read_back]);
        let error = provider
            .edit_mailbox(&super::account(), &archive())
            .await
            .unwrap_err();
        assert_eq!(error.class(), FailureClass::InvalidState);
        // The one refusal with a side effect names the mailbox it left behind.
        assert_eq!(
            rule(&error),
            Some(CreateRoleError::NotKept { mailbox: id("h") })
        );
    }
}

#[tokio::test]
async fn an_account_that_cannot_change_its_folders_is_refused_before_anything_is_sent() {
    // A read-only account: its capabilities, not the transport, decide.
    let (_provider, exec) = recording(vec![]);
    let read_only = Capabilities::none().with_mail();
    let error = crate::mailbox_write::edit_mailbox(&*exec, "c", &archive(), &read_only)
        .await
        .unwrap_err();
    assert_eq!(rule(&error), Some(CreateRoleError::NotSupported));
    assert!(exec.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_create_without_a_role_sends_no_role_at_all() {
    let (_, sent) = edit(vec![created()], &MailboxEdit::create("Receipts", None)).await;
    assert!(sent[0].1["create"]["new"].get("role").is_none(), "{sent:?}");
}

#[tokio::test]
async fn a_role_no_registry_names_is_refused_before_anything_is_sent() {
    for role in [
        MailboxRole::ServerRecords,
        MailboxRole::ChatHistory,
        MailboxRole::Other("snoozed".into()),
    ] {
        let create = MailboxEdit::create_with_role("Records", None, role);
        let (provider, exec) = recording(vec![]);
        let error = provider
            .edit_mailbox(&super::account(), &create)
            .await
            .unwrap_err();
        assert_eq!(error.class(), FailureClass::InvalidState);
        assert_eq!(rule(&error), Some(CreateRoleError::NoTransportForm));
        assert!(exec.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_role_another_folder_holds_is_a_conflict() {
    // RFC 8621 §2: one mailbox per role. Stalwart's refusal, as captured: `invalidProperties`
    // naming `role`, with prose that is never read. The tree says who holds it.
    let refusal = fixture("mailbox_set_role_refusal_response.json");
    let tree = listed(&[("s", "Sent Items", None, Some("sent"))]);
    let sent = MailboxEdit::create_with_role("Sent", None, MailboxRole::Sent);
    let (result, calls) = edit(vec![refusal, tree], &sent).await;

    assert_eq!(result, Err(FailureClass::Conflict));
    assert_eq!(calls[1].0, "Mailbox/get");
    assert_eq!(
        calls[1].1["properties"],
        json!(["name", "parentId", "role"])
    );
}

#[tokio::test]
async fn a_role_no_folder_holds_and_the_server_refuses_is_invalid_state() {
    let tree = listed(&[("i", "Inbox", None, Some("inbox"))]);
    let (result, _) = edit(
        vec![not_created("invalidProperties", &["role"]), tree],
        &archive(),
    )
    .await;
    assert_eq!(result, Err(FailureClass::InvalidState));
}

#[tokio::test]
async fn a_retry_that_meets_the_folder_its_first_attempt_made_resolves_to_it() {
    // Whichever the server checks first, the name or the role, the folder is the one asked
    // for: there by that name, under that parent, with that role.
    let mine = [("h", "Archive", None, Some("archive"))];
    for refusal in [
        not_created("alreadyExists", &[]),
        not_created("invalidProperties", &["role"]),
    ] {
        let (result, _) = edit(vec![refusal, listed(&mine)], &archive()).await;
        assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("h"))));
    }
}

#[tokio::test]
async fn a_folder_of_that_name_without_the_role_is_a_conflict_not_a_success() {
    // A create that asked for a role does not resolve to a folder that lacks it: the host
    // would take an ordinary folder for its Archive.
    let theirs = [("x", "Archive", None, None)];
    let existing = answer(
        "Mailbox/set",
        &json!({ "notCreated": { "new": { "type": "alreadyExists", "existingId": "x" } } }),
    );
    let (result, sent) = edit(vec![existing, listed(&theirs)], &archive()).await;
    assert_eq!(result, Err(FailureClass::Conflict));
    assert_eq!(
        sent.len(),
        2,
        "the existing folder's role is read, not assumed"
    );
}

#[tokio::test]
async fn an_already_exists_the_tree_does_not_explain_keeps_its_class() {
    let (result, _) = edit(
        vec![not_created("alreadyExists", &[]), listed(&[])],
        &archive(),
    )
    .await;
    assert_eq!(result, Err(FailureClass::Conflict));
}

#[tokio::test]
async fn the_folder_the_refusal_names_is_judged_before_one_found_by_name() {
    // A server may normalise the name, so the folder `existingId` names is the one to judge.
    let refusal = answer(
        "Mailbox/set",
        &json!({ "notCreated": { "new": { "type": "alreadyExists", "existingId": "x" } } }),
    );
    let normalised = [("x", "archive", None, Some("archive"))];
    let (result, _) = edit(vec![refusal.clone(), listed(&normalised)], &archive()).await;
    assert_eq!(result, Ok(MailboxEditReceipt::resolved(id("x"))));

    // And it decides even where a folder of the exact name has the role.
    let both = [
        ("x", "archive", None, None),
        ("y", "Archive", None, Some("archive")),
    ];
    let (result, _) = edit(vec![refusal, listed(&both)], &archive()).await;
    assert_eq!(result, Err(FailureClass::Conflict));
}
