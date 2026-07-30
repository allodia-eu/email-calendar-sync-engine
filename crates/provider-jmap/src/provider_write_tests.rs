//! Provider write tests: submission (context resolve → send, attachment upload,
//! missing upload URL) and mail edits (mark-seen, delete, set-error conflict) —
//! driven offline by the shared `provider_test_support` harness.

use serde_json::json;

use super::{provider_test_support::*, *};

#[tokio::test]
async fn submit_email_resolves_context_then_sends() {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::Draft;

    // Two requests: resolve Drafts/Sent + identity, then create + submit.
    let p = provider(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ]);
    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::named("Alice", "alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "Step 4 submission probe",
        "Hello",
    );
    let receipt = p.submit_email(&account(), &draft).await.unwrap();
    assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
    assert_eq!(
        receipt.message_id.as_str(),
        "step4-send-probe-0002@test.local"
    );
}

#[tokio::test]
async fn the_submission_identity_is_matched_to_the_drafts_from_address() {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::Draft;

    // The context fixture is the real `Identity/get` of an account that belongs to a shared
    // mailbox: **two** identities, and Stalwart lists the group's ahead of the user's own.
    // Taking the first — which this used to do — submitted `From: alice@` under the
    // `support@` identity, and the server answered `forbiddenFrom`. Found live; pinned here.
    let context = || fixture("submit_context_shared_identities_response.json");
    let send = |from: &str| {
        let exec = FakeExecutor::new(vec![context(), fixture("submit_send_response.json")]);
        let draft = Draft::new(
            MessageIdHeader::new("identity-probe@test.local").unwrap(),
            EmailAddress::new(from),
            vec![EmailAddress::new("bob@test.local")],
            "Identity probe",
            "Hello",
        );
        async move {
            crate::submit::send(&exec, "c", "c", &draft).await.unwrap();
            let requests = exec.requests.lock().unwrap();
            requests[1]["methodCalls"][1][1]["create"]["sub"]["identityId"]
                .as_str()
                .expect("identityId")
                .to_owned()
        }
    };

    // Sending as the user picks the user's identity, not the first in the list…
    assert_eq!(send("alice@test.local").await, "c");
    // …and sending *as the shared mailbox* picks the group's, which is the whole point of
    // having more than one.
    assert_eq!(send("support@test.local").await, "b");
    // Addresses are case-insensitive in the domain by definition (RFC 5321 §2.3.11), and no
    // mail system distinguishes the local part in practice.
    assert_eq!(send("Support@Test.Local").await, "b");
    // An address the server lists no identity for falls back to the first, so the server
    // gets to refuse it rather than this code guessing.
    assert_eq!(send("stranger@example.test").await, "b");
}

#[tokio::test]
async fn submit_email_uploads_attachment_bytes_before_sending() {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::{Draft, DraftAttachment};

    // The upload endpoint hands back a blobId, then the two-step send proceeds. Drive
    // `submit::send` directly so the fake's recorded uploads can be inspected after.
    let exec = FakeExecutor::new(vec![
        fixture("submit_context_response.json"),
        fixture("submit_send_response.json"),
    ])
    .with_upload_blob_ids(["blob-att-1"]);

    let draft = Draft::new(
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "With attachment",
        "See attached.",
    )
    .with_attachment(DraftAttachment::attachment(
        "report.pdf",
        "application/pdf",
        vec![9, 8, 7],
    ));
    crate::submit::send(&exec, "c", "c", &draft).await.unwrap();

    // The attachment bytes were POSTed to the resolved (account-substituted) upload URL
    // with the right media type — before the Email/set that references the blob.
    let uploads = exec.uploads.lock().unwrap();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].0, "http://127.0.0.1:18080/upload/c/");
    assert_eq!(uploads[0].1, "application/pdf");
    assert_eq!(uploads[0].2, vec![9, 8, 7]);
}

#[tokio::test]
async fn submit_with_attachment_but_no_upload_url_is_a_session_error() {
    use engine_core::{error::FailureClass, ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::{Draft, DraftAttachment, Provider};

    // A server without an uploadUrl cannot take attachments — a clear, permanent error.
    let p = JmapProvider::with_executor(Box::new(FakeExecutor::from_session(
        &json!({
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {}
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "c",
                "urn:ietf:params:jmap:submission": "c"
            },
            "apiUrl": "https://mail.test.local/jmap/"
        }),
        vec![],
    )));
    let draft = Draft::new(
        MessageIdHeader::new("m@test.local").unwrap(),
        EmailAddress::new("alice@test.local"),
        vec![EmailAddress::new("bob@test.local")],
        "x",
        "y",
    )
    .with_attachment(DraftAttachment::attachment(
        "r.pdf",
        "application/pdf",
        vec![1],
    ));
    let err = p.submit_email(&account(), &draft).await.unwrap_err();
    assert_eq!(err.class(), FailureClass::Permanent);
}

#[tokio::test]
async fn edit_mail_marks_seen_through_the_real_set_flow() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    // A writable mail account advertises mail writes.
    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "updated": { "eaaaaab": null } }, "0"]]
    })]);
    assert!(p.connection_info().capabilities.mail_writes());

    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::mark_seen(key.clone(), true))
        .await
        .unwrap();
    // The JMAP id is stable across the edit — the receipt echoes it.
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_delete_destroys_via_set() {
    use engine_core::ids::ProviderKey;
    use engine_provider::MailEdit;

    let p = provider(vec![json!({
        "methodResponses": [["Email/set", { "destroyed": ["eaaaaab"] }, "0"]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let receipt = p
        .edit_mail(&account(), &MailEdit::delete(key.clone()))
        .await
        .unwrap();
    assert_eq!(receipt.message_key, key);
}

#[tokio::test]
async fn edit_mail_set_error_surfaces_as_a_conflict() {
    use engine_core::{error::FailureClass, ids::ProviderKey};
    use engine_provider::MailEdit;

    // The target was destroyed server-side since it synced: a `notFound` SetError.
    let p = provider(vec![json!({
        "methodResponses": [[
            "Email/set",
            { "notUpdated": { "eaaaaab": { "type": "notFound" } } },
            "0"
        ]]
    })]);
    let key = ProviderKey::new("eaaaaab").unwrap();
    let err = p
        .edit_mail(&account(), &MailEdit::set_flagged(key, true))
        .await
        .unwrap_err();
    // Conflict → the caller re-syncs (tombstoning the gone message), then retries.
    assert_eq!(err.class(), FailureClass::Conflict);
}
