//! Mail submission via `POST /me/sendMail` in **MIME format**.
//!
//! Graph's `sendMail` accepts either a JSON `message` resource or a raw RFC 5322
//! message (base64, `Content-Type: text/plain`). The adapter uses the **MIME** form
//! for the same reason IMAP submits raw bytes: it preserves the caller's
//! pre-generated `Message-ID` verbatim (the JSON form lets the server mint its own,
//! breaking the Write Contract's reconcile-by-`Message-ID` — `store-and-sync.md`) and
//! carries `In-Reply-To`/`References`, `Cc`/`Bcc`, an HTML alternative and attachments
//! through the one shared assembler (`engine-rfc5322`).
//!
//! `sendMail` answers `202 Accepted` with **no body**, so — exactly like SMTP — there
//! is no server id for the sent copy. The receipt carries a `Message-ID`-derived
//! placeholder key; the real sent object reconciles by `Message-ID` when Sent Items
//! next syncs. Graph files the Sent copy itself (and strips the `Bcc` header before
//! delivery), so the adapter uses the **filed** assembly variant — the copy keeps its
//! `Bcc` record while no recipient can see it. This is only the provider side effect;
//! durability and idempotency are the caller's outbox (`engine-sync`).

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, ProviderResult, SubmissionReceipt};
use time::OffsetDateTime;

use crate::transport::GraphClient;

/// Sends `draft`: assembles the RFC 5322 message, base64-encodes it, and `POST`s it to
/// `sendMail` in MIME format.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError): an assembly failure
/// (a header value carrying CR/LF/NUL) is permanent; a `400 ErrorMimeContentInvalidBase64String`
/// is permanent; `401`/`429`/`5xx` classify as auth/rate-limit/retryable.
pub(crate) async fn send(client: &GraphClient, draft: &Draft) -> ProviderResult<SubmissionReceipt> {
    // The filed variant keeps the `Bcc` header: Graph reads every recipient (To/Cc/Bcc)
    // from the MIME to build the delivery envelope and strips `Bcc` before delivering,
    // so the Sent-Items copy records whom the sender Bcc'd while no recipient sees it.
    let mime = engine_rfc5322::assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    // sendMail MIME format: the whole message as a base64 `text/plain` body.
    let body = engine_rfc5322::base64_encode(&mime).into_bytes();
    client
        .post(&client.url("/sendMail"), "text/plain", body)
        .await?;
    Ok(SubmissionReceipt::new(
        sent_placeholder_key(draft),
        draft.message_id.clone(),
    ))
}

/// The placeholder key for the sent copy — `sent:<Message-ID>` — mirroring IMAP's
/// no-`UIDPLUS` filing key. `sendMail` returns no id, so this stands in until the sent
/// message syncs back from Sent Items and reconciles by `Message-ID`.
fn sent_placeholder_key(draft: &Draft) -> ProviderKey {
    ProviderKey::new(format!("sent:{}", draft.message_id.as_str()))
        .expect("a Message-ID-derived placement key is never empty")
}

#[cfg(test)]
mod tests {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};

    use super::*;
    use crate::test_support::fake_client_fallible;

    fn draft() -> Draft {
        Draft::new(
            MessageIdHeader::new("graph-send-0001@test.local").unwrap(),
            EmailAddress::new("allodia-e2e@outlook.com"),
            vec![EmailAddress::new("bob@test.local")],
            "Subject",
            "Body",
        )
    }

    #[tokio::test]
    async fn send_posts_to_sendmail_and_echoes_message_id() {
        // A 202-no-body route (`Value::Null`) models a successful sendMail.
        let client = fake_client_fallible(vec![("/sendMail", Ok(serde_json::Value::Null))]);
        let receipt = send(&client, &draft()).await.unwrap();
        // The sent copy has no server id, so the key is Message-ID-derived and the
        // Message-ID is echoed for sync-time reconciliation.
        assert_eq!(
            receipt.email_key.as_str(),
            "sent:graph-send-0001@test.local"
        );
        assert_eq!(receipt.message_id.as_str(), "graph-send-0001@test.local");
    }

    #[tokio::test]
    async fn a_malformed_mime_rejection_is_permanent() {
        use engine_core::error::FailureClass;
        // Graph's documented 400 for a bad MIME body classifies as permanent (never retried).
        let body = serde_json::json!({
            "error": { "code": "ErrorMimeContentInvalidBase64String", "message": "bad" }
        });
        let client = fake_client_fallible(vec![("/sendMail", Err((400, body)))]);
        let err = send(&client, &draft()).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
    }

    #[tokio::test]
    async fn a_header_injection_in_the_draft_is_rejected_before_any_request() {
        use engine_core::error::FailureClass;
        // A subject carrying CRLF must be refused at assembly, not sent — even with a
        // route that would accept anything.
        let client = fake_client_fallible(vec![("/sendMail", Ok(serde_json::Value::Null))]);
        let mut poisoned = draft();
        poisoned.subject = "Hi\r\nBcc: victim@evil.example".to_owned();
        let err = send(&client, &poisoned).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
    }

    #[tokio::test]
    async fn send_posts_a_text_plain_base64_mime_over_the_real_transport() {
        use engine_provider::ContentIdHeader;

        use crate::{
            GraphClient,
            test_support::{base64_decode, capturing_server, tls},
        };

        // Drive the REAL reqwest transport (via `with_base`) at a capturing server, so
        // the offline suite asserts the actual request shape the Fake can't (`AGENTS.md`).
        let (base, rx) = capturing_server("202 Accepted", "");
        let client = GraphClient::with_base("secret-token", base, tls()).unwrap();

        let draft = draft()
            .with_cc(vec![EmailAddress::new("carol@test.local")])
            .with_bcc(vec![EmailAddress::new("dave@test.local")])
            .with_html_body("<p>Body</p>")
            .with_attachment(engine_provider::DraftAttachment::inline(
                "c.png",
                "image/png",
                ContentIdHeader::new("c1@test.local").unwrap(),
                vec![1, 2, 3],
            ));
        let receipt = send(&client, &draft).await.unwrap();
        assert_eq!(receipt.message_id.as_str(), "graph-send-0001@test.local");

        let request = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the capturing server received the request");
        let lower = request.to_ascii_lowercase();
        // The verb + endpoint + the MIME-format signal (text/plain), and the bearer.
        assert!(request.starts_with("POST /me/sendMail "), "{request}");
        assert!(lower.contains("content-type: text/plain"), "{request}");
        assert!(lower.contains("authorization: bearer secret-token"));

        // The body is base64 that decodes to the assembled MIME — preserving the caller's
        // Message-ID and threading (the whole reason for the MIME form), and carrying the
        // Bcc (the filed variant) plus the HTML alternative and the inline attachment.
        let b64 = request.split("\r\n\r\n").nth(1).expect("a request body");
        let mime = String::from_utf8(base64_decode(b64)).unwrap();
        assert!(
            mime.contains("Message-ID: <graph-send-0001@test.local>"),
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
}
