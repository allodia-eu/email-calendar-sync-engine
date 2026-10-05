//! A submission through the real client: the attachment's blob goes up before the request that
//! submits.

use super::{RICH_SESSION_DOC, http_ok, mock_server, rich_config};
use crate::JmapProvider;

#[tokio::test]
async fn submit_email_uploads_the_attachment_blob_through_the_real_client() {
    use engine_core::{
        ids::{AccountId, MessageIdHeader},
        mail::EmailAddress,
    };
    use engine_provider::{Draft, DraftAttachment, HandOver, Provider, Unrecorded};

    // connect(session) → resolve_context(Mailbox/Identity) → upload(blob) → send.
    let context = r#"{"methodResponses":[["Mailbox/get",{"list":[{"id":"d","name":"Drafts","role":"drafts"},{"id":"s","name":"Sent","role":"sent"}]},"0"],["Identity/get",{"list":[{"id":"id1"}]},"1"]]}"#;
    let sent = r#"{"methodResponses":[["Email/set",{"created":{"draft":{"id":"e9"}}},"0"],["EmailSubmission/set",{"created":{"sub":{"id":"sub1"}}},"1"]]}"#;
    let base = mock_server(vec![
        http_ok(RICH_SESSION_DOC),
        http_ok(context),
        http_ok(r#"{"blobId":"blob-att","type":"application/pdf","size":3}"#),
        http_ok(sent),
    ]);
    let provider = JmapProvider::connect(rich_config(base)).await.unwrap();

    let draft = Draft::new(
        MessageIdHeader::new("m@test.local").unwrap(),
        EmailAddress::new("a@test.local"),
        vec![EmailAddress::new("b@test.local")],
        "subject",
        "body",
    )
    .with_attachment(DraftAttachment::attachment(
        "r.pdf",
        "application/pdf",
        vec![1, 2, 3],
    ));
    let receipt = provider
        .submit_email(
            &AccountId::try_from("acct").unwrap(),
            &draft,
            &HandOver::new(&Unrecorded),
        )
        .await
        .unwrap();
    assert_eq!(receipt.email_key.as_str(), "e9");
}
