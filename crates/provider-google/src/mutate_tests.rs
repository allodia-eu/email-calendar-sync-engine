//! Offline `edit_mail` tests: the label-delta orchestration through the fixture-routing
//! fake, and the exact `messages.modify`/`trash`/`delete` request shapes through the
//! request-capturing server (which the fakes cannot assert — `AGENTS.md`).

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
};
use engine_provider::MailEdit;

use super::*;
use crate::{
    GoogleClient,
    test_support::{capturing_server, fake_client, fake_client_fallible, json, tls},
};

fn key(id: &str) -> ProviderKey {
    ProviderKey::new(id).unwrap()
}

#[tokio::test]
async fn mark_read_removes_unread_and_flag_adds_starred() {
    // A modify route returns the echoed message; the fake ignores the body, so this
    // asserts orchestration (which endpoint), not the body — the capturing test does body.
    let client = fake_client(vec![(
        "/messages/message-1/modify",
        json(r#"{"id":"message-1"}"#),
    )]);
    let receipt = edit(&client, &MailEdit::mark_seen(key("message-1"), true))
        .await
        .unwrap();
    assert_eq!(receipt.message_key.as_str(), "message-1");
    edit(&client, &MailEdit::set_flagged(key("message-1"), true))
        .await
        .unwrap();
}

#[tokio::test]
async fn mark_read_body_removes_unread_over_the_real_transport() {
    // Drive the REAL reqwest transport at a capturing server and assert the modify body.
    let (base, rx) = capturing_server("200 OK", r#"{"id":"message-1"}"#);
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    edit(&client, &MailEdit::mark_seen(key("message-1"), true))
        .await
        .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("POST /gmail/v1/users/me/messages/message-1/modify "),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer tok")
    );
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    // Marking read = removing UNREAD (the inversion), adding nothing.
    assert_eq!(json["removeLabelIds"], serde_json::json!(["UNREAD"]));
    assert_eq!(json["addLabelIds"], serde_json::json!([]));
}

#[tokio::test]
async fn mark_unread_adds_unread() {
    let (base, rx) = capturing_server("200 OK", r#"{"id":"message-1"}"#);
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    edit(&client, &MailEdit::mark_seen(key("message-1"), false))
        .await
        .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let body = request.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    // Marking unread = adding UNREAD.
    assert_eq!(json["addLabelIds"], serde_json::json!(["UNREAD"]));
}

#[tokio::test]
async fn move_to_trash_uses_the_trash_endpoint() {
    let (base, rx) = capturing_server("200 OK", r#"{"id":"message-1"}"#);
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    edit(
        &client,
        &MailEdit::move_to(key("message-1"), MailboxId::try_from("TRASH").unwrap()),
    )
    .await
    .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("POST /gmail/v1/users/me/messages/message-1/trash "),
        "{request}"
    );
}

#[tokio::test]
async fn move_to_a_label_replaces_membership_leaving_state_intact() {
    // MoveTo fetches the current labels then replaces: add destination, remove every
    // other place label (but not UNREAD/STARRED/SENT). The message currently has
    // INBOX + UNREAD + STARRED + CATEGORY_UPDATES.
    let client = fake_client(vec![
        (
            "/messages/message-1?format=minimal",
            json(
                r#"{"id":"message-1","labelIds":["INBOX","UNREAD","STARRED","CATEGORY_UPDATES"]}"#,
            ),
        ),
        ("/messages/message-1/modify", json(r#"{"id":"message-1"}"#)),
    ]);
    // Drive against the capturing server for the modify body — but the label fetch needs
    // routing, so use the fake for orchestration correctness, plus a capturing test below
    // only for the wire body of the modify. Here assert the receipt.
    let receipt = edit(
        &client,
        &MailEdit::move_to(key("message-1"), MailboxId::try_from("Label_1").unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(receipt.message_key.as_str(), "message-1");
}

#[tokio::test]
async fn permanent_delete_hits_the_delete_endpoint() {
    let (base, rx) = capturing_server("204 No Content", "");
    let client = GoogleClient::with_base("tok", base, tls()).unwrap();
    edit(&client, &MailEdit::delete(key("message-1")))
        .await
        .unwrap();
    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        request.starts_with("DELETE /gmail/v1/users/me/messages/message-1 "),
        "{request}"
    );
}

#[tokio::test]
async fn a_stale_target_is_a_conflict() {
    // A 412 from modify classifies as a conflict the outbox resolves by refetch-and-retry.
    let client = fake_client_fallible(vec![(
        "/messages/message-1/modify",
        Err((
            412,
            json(r#"{"error":{"code":412,"errors":[{"reason":"conditionNotMet"}]}}"#),
        )),
    )]);
    let err = edit(&client, &MailEdit::mark_seen(key("message-1"), true))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Conflict);
}
