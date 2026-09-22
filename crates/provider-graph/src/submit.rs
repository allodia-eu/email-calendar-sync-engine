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
    check_from_matches_principal(client, draft)?;
    // The filed variant keeps the `Bcc` header: Graph reads every recipient (To/Cc/Bcc)
    // from the MIME to build the delivery envelope and strips `Bcc` before delivering,
    // so the Sent-Items copy records whom the sender Bcc'd while no recipient sees it.
    let mime = engine_rfc5322::assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    // sendMail MIME format: the whole message as a base64 `text/plain` body.
    let body = engine_rfc5322::base64_encode(&mime).into_bytes();
    client
        .post(&client.url("/sendMail"), "text/plain", body)
        .await?;
    Ok(SubmissionReceipt::filed(
        sent_placeholder_key(draft),
        draft.message_id.clone(),
    ))
}

/// Refuses a draft whose `From` names another mailbox than the one this client posts to.
///
/// `sendMail` goes to the client's principal, so a client bound to a shared mailbox
/// (`/users/{shared}/sendMail`) sends *from that mailbox* whatever the draft's `From`
/// says — and a message whose header disagrees with its sender misrepresents who wrote it.
/// How Exchange treats the mismatch is not something to rely on, so it never reaches
/// Exchange. Sending as the shared mailbox is exactly what such a client is for; Exchange
/// then adds a `Sender:` naming the signed-in delegate, which clients render as "on behalf
/// of" (`graph.md`).
///
/// Only a **named** principal can be checked. A client bound to `/me` does not know its own
/// address without asking the directory, and its `From` may legitimately differ anyway —
/// a delegate with Send As sends as a shared address through their own mailbox.
///
/// This refuses a `From` that is an **alias** of the bound mailbox, which Exchange itself
/// would accept: telling an alias from a mistake needs the mailbox's `proxyAddresses`, a
/// directory read. The exact workaround is to bind a client to the alias — `/users/{alias}`
/// resolves to the same mailbox, and then endpoint and header agree.
fn check_from_matches_principal(client: &GraphClient, draft: &Draft) -> ProviderResult<()> {
    let Some(mailbox) = client.principal().address() else {
        return Ok(());
    };
    if mailbox.eq_ignore_ascii_case(&draft.from.email) {
        return Ok(());
    }
    Err(engine_provider::ProviderError::permanent(format!(
        "this client sends from {mailbox:?}, not from the draft's {:?}; bind a client to \
         the mailbox the message is from",
        draft.from.email
    )))
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
    async fn a_client_bound_to_a_shared_mailbox_sends_only_as_that_mailbox() {
        use engine_core::error::FailureClass;

        use crate::MailboxPrincipal;

        // The route would accept anything, so the refusal has to happen before the request:
        // a message whose `From` does not match the mailbox it leaves from is never sent.
        let bound = fake_client_fallible(vec![("/sendMail", Ok(serde_json::Value::Null))])
            .with_principal(MailboxPrincipal::user("shared@example.test").unwrap());
        let err = send(&bound, &draft()).await.unwrap_err();
        assert_eq!(err.class(), FailureClass::Permanent);
        assert!(err.detail().contains("shared@example.test"), "{err}");

        // Sending *as* the bound mailbox is what it is for, in any case.
        let mut as_shared = draft();
        as_shared.from = EmailAddress::new("Shared@Example.Test");
        assert!(send(&bound, &as_shared).await.is_ok());

        // A client on `/me` has no address to compare — and a Send As delegate legitimately
        // sends a shared address through their own mailbox — so the draft goes as given.
        let own = fake_client_fallible(vec![("/sendMail", Ok(serde_json::Value::Null))]);
        assert!(send(&own, &draft()).await.is_ok());
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
            test_support::{base64_decode, capturing_server, retry, tls},
        };

        // Drive the REAL reqwest transport (via `with_base`) at a capturing server, so
        // the offline suite asserts the actual request shape the Fake can't (`AGENTS.md`).
        let (base, rx) = capturing_server("202 Accepted", "");
        let client = GraphClient::with_base("secret-token", base, tls(), &retry()).unwrap();

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
