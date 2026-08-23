//! Offline tests for mail writes: the PATCH keyword-body builder, the move/delete
//! flows over the fake transport (routed to the captured real responses), delete
//! idempotency, and the real request shapes asserted through a capturing server.

use engine_core::{
    error::FailureClass,
    ids::{MailboxId, ProviderKey},
    mail::{Keyword, SystemKeyword},
};
use engine_provider::MailEdit;

use super::*;
use crate::{
    GraphClient,
    test_support::{capturing_server, fake_client, fake_client_fallible, json, retry, tls},
};

const PATCHED: &str = include_str!("../tests/fixtures/mail/message_patched.json");
const MOVED: &str = include_str!("../tests/fixtures/mail/message_moved.json");

fn target() -> ProviderKey {
    ProviderKey::new("message-write").unwrap()
}

fn set(keywords: &[SystemKeyword]) -> std::collections::BTreeSet<Keyword> {
    keywords.iter().map(|k| Keyword::system(*k)).collect()
}

#[test]
fn keyword_patch_maps_seen_and_flagged() {
    // Add $seen + $flagged → isRead:true and flag flagged.
    let body = keyword_patch(
        &set(&[SystemKeyword::Seen, SystemKeyword::Flagged]),
        &set(&[]),
    )
    .unwrap();
    assert_eq!(body["isRead"], serde_json::json!(true));
    assert_eq!(body["flag"]["flagStatus"], "flagged");
}

#[test]
fn keyword_patch_clears_map_to_false_and_notflagged() {
    // Remove $seen + $flagged → isRead:false and flag notFlagged.
    let body = keyword_patch(
        &set(&[]),
        &set(&[SystemKeyword::Seen, SystemKeyword::Flagged]),
    )
    .unwrap();
    assert_eq!(body["isRead"], serde_json::json!(false));
    assert_eq!(body["flag"]["flagStatus"], "notFlagged");
}

#[test]
fn keyword_patch_rejects_a_keyword_graph_cannot_write() {
    // $draft is read-only and any user keyword has no Graph property — rejected, never a
    // silent no-op the caller reads as success.
    for kw in [SystemKeyword::Draft, SystemKeyword::Answered] {
        let err = keyword_patch(&set(&[kw]), &set(&[])).unwrap_err();
        assert_eq!(err.class(), FailureClass::InvalidState);
    }
    let custom: std::collections::BTreeSet<Keyword> = [Keyword::new("work").unwrap()].into();
    let err = keyword_patch(&custom, &set(&[])).unwrap_err();
    assert_eq!(err.class(), FailureClass::InvalidState);
}

#[tokio::test]
async fn set_keywords_with_both_sides_empty_is_a_no_op() {
    // No keywords to change → no request issued, but the pending op still resolves.
    let client = fake_client_fallible(vec![]);
    let edit = MailEdit::SetKeywords {
        target: target(),
        add: set(&[]),
        remove: set(&[]),
    };
    let receipt = edit_mail(&client, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());
}

#[tokio::test]
async fn set_keywords_patches_and_returns_the_target_key() {
    // Routed to the captured real PATCH response; the receipt carries the unchanged key.
    let client = fake_client(vec![("/messages/message-write", json(PATCHED))]);
    let edit = MailEdit::mark_seen(target(), true);
    let receipt = edit_mail(&client, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());
}

#[tokio::test]
async fn move_returns_the_unchanged_target_key() {
    // Immutable ids are stable across a move (captured response confirms it), so the
    // receipt key is the target; the destination reconciles on its next sync.
    let client = fake_client(vec![("/messages/message-write/move", json(MOVED))]);
    let edit = MailEdit::move_to(target(), MailboxId::try_from("folder-archive").unwrap());
    let receipt = edit_mail(&client, &edit).await.unwrap();
    assert_eq!(receipt.message_key, target());
}

#[tokio::test]
async fn delete_succeeds_and_is_idempotent_on_404() {
    // A 204 (no body) → success.
    let ok = fake_client(vec![("/permanentDelete", serde_json::Value::Null)]);
    assert_eq!(
        edit_mail(&ok, &MailEdit::delete(target()))
            .await
            .unwrap()
            .message_key,
        target()
    );
    // Already gone (404) → idempotent success.
    let gone = fake_client_fallible(vec![("/permanentDelete", Err((404, json("{}"))))]);
    assert!(edit_mail(&gone, &MailEdit::delete(target())).await.is_ok());
}

#[tokio::test]
async fn delete_propagates_an_ambiguous_re_delete() {
    // The re-delete of an already-purged message is `403 ErrorCannotDeleteObject`, not a
    // clean 404; it propagates (the outbox's NeedsConfirmation owns the ambiguous retry).
    let body =
        serde_json::json!({ "error": { "code": "ErrorCannotDeleteObject", "message": "no" } });
    let client = fake_client_fallible(vec![("/permanentDelete", Err((403, body)))]);
    let err = edit_mail(&client, &MailEdit::delete(target()))
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn writes_send_the_expected_request_shapes_over_the_real_transport() {
    // The offline fake ignores request bodies (`AGENTS.md`), so drive the REAL reqwest
    // transport at a capturing server to assert the actual PATCH/move/delete shapes.

    // 1. SetKeywords → PATCH /me/messages/{id} with { isRead, flag }.
    let (base, rx) = capturing_server("200 OK", PATCHED);
    let client = GraphClient::with_base("tok", base, tls(), retry()).unwrap();
    let edit = MailEdit::SetKeywords {
        target: target(),
        add: set(&[SystemKeyword::Seen, SystemKeyword::Flagged]),
        remove: set(&[]),
    };
    edit_mail(&client, &edit).await.unwrap();
    let req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        req.starts_with("PATCH /me/messages/message-write "),
        "{req}"
    );
    let body = req.split("\r\n\r\n").nth(1).unwrap();
    let sent: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(sent["isRead"], serde_json::json!(true));
    assert_eq!(sent["flag"]["flagStatus"], "flagged");

    // 2. MoveTo → POST /me/messages/{id}/move with { destinationId }.
    let (base, rx) = capturing_server("201 Created", MOVED);
    let client = GraphClient::with_base("tok", base, tls(), retry()).unwrap();
    let edit = MailEdit::move_to(target(), MailboxId::try_from("folder-archive").unwrap());
    edit_mail(&client, &edit).await.unwrap();
    let req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        req.starts_with("POST /me/messages/message-write/move "),
        "{req}"
    );
    let body = req.split("\r\n\r\n").nth(1).unwrap();
    let sent: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(sent["destinationId"], "folder-archive");

    // 3. Delete → POST /me/messages/{id}/permanentDelete (empty body).
    let (base, rx) = capturing_server("204 No Content", "");
    let client = GraphClient::with_base("tok", base, tls(), retry()).unwrap();
    edit_mail(&client, &MailEdit::delete(target()))
        .await
        .unwrap();
    let req = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert!(
        req.starts_with("POST /me/messages/message-write/permanentDelete "),
        "{req}"
    );
    // The bodyless action must carry an explicit `Content-Length: 0`, or Graph answers
    // `411 Length Required` (reqwest omits the header for an empty body).
    assert!(
        req.to_ascii_lowercase().contains("content-length: 0"),
        "{req}"
    );
}
