//! Offline submission tests: the `messages.send` orchestration through the fake, and the
//! exact request shape (base64url MIME `raw` field) through the capturing server.

use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
use engine_provider::{ContentIdHeader, Draft, DraftAttachment};

use super::*;
use crate::{
    GoogleClient, base64url,
    test_support::{capturing_server, fake_client_fallible, retry, tls},
};

fn draft() -> Draft {
    Draft::new(
        MessageIdHeader::new("gmail-send-0001@test.local").unwrap(),
        EmailAddress::new("testuser@example.test"),
        vec![EmailAddress::new("bob@test.local")],
        "Subject",
        "Body",
    )
}

#[tokio::test]
async fn send_returns_the_real_gmail_id_as_the_key() {
    // Gmail's send echoes the sent message's id, so the receipt key is that id directly
    // (no reconcile-by-Message-ID, which Gmail's Message-ID rewrite would defeat anyway).
    let client = fake_client_fallible(vec![(
        "/messages/send",
        Ok(serde_json::json!({ "id": "19f7abcdef012345", "threadId": "19f7abcdef012345" })),
    )]);
    let receipt = send(&client, &draft()).await.unwrap();
    assert_eq!(receipt.email_key.as_str(), "19f7abcdef012345");
    assert_eq!(receipt.message_id.as_str(), "gmail-send-0001@test.local");
}

#[tokio::test]
async fn a_header_injection_in_the_draft_is_rejected_before_any_request() {
    use engine_core::error::FailureClass;
    let client = fake_client_fallible(vec![(
        "/messages/send",
        Ok(serde_json::json!({ "id": "x" })),
    )]);
    let mut poisoned = draft();
    poisoned.subject = "Hi\r\nBcc: victim@evil.example".to_owned();
    let err = send(&client, &poisoned).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn send_posts_a_base64url_raw_mime_over_the_real_transport() {
    // Drive the REAL reqwest transport at a capturing server, so the offline suite asserts
    // the request shape the fake cannot (`AGENTS.md`).
    let (base, rx) = capturing_server("200 OK", r#"{"id":"19f7abcdef012345"}"#);
    let client = GoogleClient::with_base("secret-token", base, tls(), retry()).unwrap();

    let draft = draft()
        .with_cc(vec![EmailAddress::new("carol@test.local")])
        .with_bcc(vec![EmailAddress::new("dave@test.local")])
        .with_html_body("<p>Body</p>")
        .with_attachment(DraftAttachment::inline(
            "c.png",
            "image/png",
            ContentIdHeader::new("c1@test.local").unwrap(),
            vec![1, 2, 3],
        ));
    let receipt = send(&client, &draft).await.unwrap();
    assert_eq!(receipt.email_key.as_str(), "19f7abcdef012345");

    let request = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    let lower = request.to_ascii_lowercase();
    assert!(
        request.starts_with("POST /gmail/v1/users/me/messages/send "),
        "{request}"
    );
    assert!(
        lower.contains("content-type: application/json"),
        "{request}"
    );
    assert!(lower.contains("authorization: bearer secret-token"));

    // The JSON body carries the whole MIME as a base64url `raw` field, which decodes to
    // the assembled message: the caller's Message-ID, threading, Cc/Bcc, the HTML
    // alternative, and the inline attachment.
    let body = request.split("\r\n\r\n").nth(1).expect("a body");
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    let raw = json["raw"].as_str().expect("a raw field");
    let mime = String::from_utf8(base64url::decode(raw).unwrap()).unwrap();
    assert!(
        mime.contains("Message-ID: <gmail-send-0001@test.local>"),
        "{mime}"
    );
    assert!(mime.contains("Cc: carol@test.local"), "{mime}");
    assert!(mime.contains("Bcc: dave@test.local"), "{mime}");
    assert!(
        mime.contains("Content-Type: multipart/alternative"),
        "{mime}"
    );
    assert!(mime.contains("Content-ID: <c1@test.local>"), "{mime}");
}

#[tokio::test]
async fn send_falls_back_to_a_message_id_key_when_no_id_is_returned() {
    // If the send response somehow carried no id, the receipt key is Message-ID-derived
    // (the placeholder the sent copy reconciles against on the next Sent sync).
    let client = fake_client_fallible(vec![("/messages/send", Ok(serde_json::Value::Null))]);
    let receipt = send(&client, &draft()).await.unwrap();
    assert_eq!(
        receipt.email_key.as_str(),
        "sent:gmail-send-0001@test.local"
    );
}

#[tokio::test]
async fn a_rate_limit_on_send_is_retryable() {
    use engine_core::error::FailureClass;
    let client = fake_client_fallible(vec![(
        "/messages/send",
        Err((
            429,
            serde_json::json!({ "error": { "code": 429, "status": "RESOURCE_EXHAUSTED" } }),
        )),
    )]);
    let err = send(&client, &draft()).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::RateLimited);
}
