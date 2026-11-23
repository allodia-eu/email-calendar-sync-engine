//! The import as a whole, offline: its limits, its cursor, and how a batch is routed, inline
//! or by the upload URL. `batch_tests` covers packing and failures. The captured answers prove
//! what a server says; `tests/live_import.rs` proves it takes these requests.

use engine_core::{error::FailureClass, ids::MailboxId};
use engine_provider::Provider;
use serde_json::{Value, json};

use super::{scripted::*, *};
use crate::provider::provider_test_support::fixture;

#[tokio::test]
async fn the_limits_are_the_session_s() {
    let (provider, _) = target(session(|_| {}), vec![], vec![]);
    let limits = provider.mail_import_limits(&account()).await.unwrap();
    assert_eq!(limits.max_message_size().unwrap().get(), 50_000_000);
    assert_eq!(limits.max_mailboxes().get(), 100);

    // No stated mailbox limit, and no upload URL: the largest blob one request takes.
    let (provider, _) = target(
        session(|doc| {
            doc["accounts"]["c"]["accountCapabilities"]["urn:ietf:params:jmap:mail"]["maxMailboxesPerEmail"] =
                Value::Null;
            doc.as_object_mut().unwrap().remove("uploadUrl");
        }),
        vec![],
        vec![],
    );
    let limits = provider.mail_import_limits(&account()).await.unwrap();
    assert_eq!(limits.max_message_size().unwrap().get(), 7_499_488);
    assert_eq!(limits.max_mailboxes(), std::num::NonZeroUsize::MAX);

    // Neither an upload URL nor the blob extension: no size the target can take is stated.
    let (provider, _) = target(
        session(|doc| {
            doc.as_object_mut().unwrap().remove("uploadUrl");
            doc["accounts"]["c"]["accountCapabilities"]
                .as_object_mut()
                .unwrap()
                .remove("urn:ietf:params:jmap:blob");
        }),
        vec![],
        vec![],
    );
    let limits = provider.mail_import_limits(&account()).await.unwrap();
    assert_eq!(limits.max_message_size(), None);
}

#[tokio::test]
async fn a_host_reaches_the_importer_through_the_provider_where_mail_is_writable() {
    let (provider, _) = target(session(|_| {}), vec![], vec![]);
    assert!(provider.mail_importer().is_some());
    let (read_only, _) = target(
        session(|doc| doc["accounts"]["c"]["isReadOnly"] = json!(true)),
        vec![],
        vec![],
    );
    assert!(read_only.mail_importer().is_none());

    // No upload URL and no blob extension: nothing it could send would reach the server.
    let no_way_up = |doc: &mut Value| {
        doc.as_object_mut().unwrap().remove("uploadUrl");
        doc["accounts"]["c"]["accountCapabilities"]
            .as_object_mut()
            .unwrap()
            .remove("urn:ietf:params:jmap:blob");
    };
    let (closed, _) = target(session(no_way_up), vec![], vec![]);
    assert!(closed.mail_importer().is_none());
    // Either way up is enough.
    let (inline_only, _) = target(
        session(|doc| {
            doc.as_object_mut().unwrap().remove("uploadUrl");
        }),
        vec![],
        vec![],
    );
    assert!(inline_only.mail_importer().is_some());
    let (by_url_only, _) = target(
        session(|doc| {
            doc["accounts"]["c"]["accountCapabilities"]
                .as_object_mut()
                .unwrap()
                .remove("urn:ietf:params:jmap:blob");
        }),
        vec![],
        vec![],
    );
    assert!(by_url_only.mail_importer().is_some());
}

/// The cursor's failure names its code, never the server's text.
#[tokio::test]
async fn a_cursor_that_cannot_be_read_says_why_in_codes() {
    let (provider, _) = target(
        session(|_| {}),
        vec![Err(JmapError::status(
            500,
            r#"{"detail":"mailbox Secret plans"}"#,
        ))],
        vec![],
    );
    let err = provider
        .import_cursor(&account(), &MailboxId::try_from("ec").unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.class(), FailureClass::Retryable);
    assert!(!format!("{err} {err:?}").contains("Secret"), "{err:?}");
}

#[tokio::test]
async fn the_cursor_is_the_account_s_email_state_and_an_unknown_mailbox_is_refused() {
    let (provider, scripted) = target(
        session(|_| {}),
        vec![
            Ok(fixture("import_cursor_response.json")),
            Ok(fixture("import_cursor_unknown_mailbox_response.json")),
        ],
        vec![],
    );
    let mailbox = MailboxId::try_from("ec").unwrap();
    let cursor = provider.import_cursor(&account(), &mailbox).await.unwrap();
    assert_eq!(cursor.as_str(), "s0ecq");
    let sent = scripted.sent.lock().unwrap()[0].clone();
    assert_eq!(sent["methodCalls"][0][1]["ids"], json!(["ec"]));
    assert_eq!(sent["methodCalls"][1][0], "Email/get");

    let gone = MailboxId::try_from("zzzzzzz").unwrap();
    let err = provider.import_cursor(&account(), &gone).await.unwrap_err();
    assert_eq!(
        err.target_refusal(),
        Some(engine_provider::TargetRefusal::NoSuchContainer)
    );
}

#[tokio::test]
async fn an_empty_batch_sends_nothing() {
    let (provider, scripted) = target(session(|_| {}), vec![], vec![]);
    assert!(provider.import_messages(&account(), &[]).await.is_empty());
    assert!(scripted.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_message_over_a_stated_limit_is_refused_before_anything_is_sent() {
    let (provider, scripted) = target(session(|_| {}), vec![], vec![]);
    let batch = [message(10, 101), message(50_000_001, 1)];
    let answers = provider.import_messages(&account(), &batch).await;
    assert!(
        answers
            .iter()
            .all(|a| a.as_ref().unwrap_err().is_item_unavailable())
    );
    assert!(scripted.sent.lock().unwrap().is_empty());
    assert!(scripted.uploaded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn small_messages_go_inline_in_one_request_and_are_answered_in_order() {
    let (provider, scripted) = target(session(|_| {}), vec![Ok(stored(&[0, 2], &[0, 2]))], vec![]);
    let batch = [message(100, 1), message(10, 101), message(200, 2)];
    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(key(&answers[0]), "M0");
    assert!(answers[1].as_ref().unwrap_err().is_item_unavailable());
    assert_eq!(key(&answers[2]), "M2");
    let sent = scripted.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["methodCalls"][0][0], "Blob/upload");
    assert_eq!(
        sent[0]["methodCalls"][1][1]["emails"]["m2"]["blobId"],
        "#b2"
    );
    assert!(scripted.uploaded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_message_too_large_for_a_blob_goes_by_the_upload_url_first() {
    let (provider, scripted) = target(
        session(|_| {}),
        vec![Ok(stored(&[0, 1], &[1]))],
        vec![Ok("U0".to_owned())],
    );
    let batch = [message(7_499_489, 1), message(10, 1)];
    let answers = provider.import_messages(&account(), &batch).await;

    assert_eq!(key(&answers[0]), "M0");
    assert_eq!(key(&answers[1]), "M1");
    let uploaded = scripted.uploaded.lock().unwrap();
    assert_eq!(
        *uploaded,
        vec![(
            "http://127.0.0.1:18080/jmap/upload/c/".to_owned(),
            "message/rfc822".to_owned(),
            7_499_489
        )]
    );
    let sent = scripted.sent.lock().unwrap();
    let emails = &sent[0]["methodCalls"][1][1]["emails"];
    assert_eq!(emails["m0"]["blobId"], "U0");
    assert_eq!(emails["m1"]["blobId"], "#b1");
}

#[tokio::test]
async fn without_an_upload_url_a_message_that_cannot_go_inline_is_unavailable() {
    // Within `maxSizeBlobSet`, so the limits pass it, but its base64 does not fit a request.
    let (provider, scripted) = target(
        session(|doc| {
            doc.as_object_mut().unwrap().remove("uploadUrl");
            doc["capabilities"]["urn:ietf:params:jmap:core"]["maxSizeRequest"] = json!(4_000_000);
        }),
        vec![],
        vec![],
    );
    let answers = provider
        .import_messages(&account(), &[message(5_000_000, 1)])
        .await;
    let refused = answers[0].as_ref().unwrap_err();
    assert!(refused.is_item_unavailable(), "{refused}");
    assert!(refused.detail().contains("upload URL"), "{refused}");
    assert!(scripted.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_mail_account_every_message_is_answered() {
    let (provider, _) = target(
        session(|doc| {
            doc["primaryAccounts"]
                .as_object_mut()
                .unwrap()
                .remove("urn:ietf:params:jmap:mail");
        }),
        vec![],
        vec![],
    );
    let answers = provider
        .import_messages(&account(), &[message(1, 1), message(1, 1)])
        .await;
    assert_eq!(answers.len(), 2);
    assert!(answers.iter().all(Result::is_err));
    assert!(
        provider
            .import_cursor(&account(), &MailboxId::try_from("x").unwrap())
            .await
            .is_err()
    );
}
