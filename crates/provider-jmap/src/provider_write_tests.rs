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

/// Sends one draft from `from` against `context`, returning the `identityId` the
/// submission carried — or the refusal, when it never got that far.
async fn identity_sent_for(context: serde_json::Value, from: &str) -> Result<String, String> {
    use engine_core::{ids::MessageIdHeader, mail::EmailAddress};
    use engine_provider::Draft;

    let exec = FakeExecutor::new(vec![context, fixture("submit_send_response.json")]);
    let draft = Draft::new(
        MessageIdHeader::new("identity-probe@test.local").unwrap(),
        EmailAddress::new(from),
        vec![EmailAddress::new("bob@test.local")],
        "Identity probe",
        "Hello",
    );
    crate::submit::send(&exec, "c", "c", &draft)
        .await
        .map_err(|err| err.to_string())?;
    let requests = exec.requests.lock().unwrap();
    Ok(
        requests[1]["methodCalls"][1][1]["create"]["sub"]["identityId"]
            .as_str()
            .expect("identityId")
            .to_owned(),
    )
}

#[tokio::test]
async fn the_submission_identity_is_the_one_for_the_drafts_from() {
    // The captured `Identity/get` of alice after she joined the `support` group: two
    // identities, the group's listed **first**. Taking the first — which this used to do —
    // submitted `From: alice@` under `support@`, and Stalwart answered `forbiddenFrom`.
    let context = || fixture("submit_context_shared_identities_response.json");
    assert_eq!(
        identity_sent_for(context(), "alice@test.local").await,
        Ok("c".into())
    );
    assert_eq!(
        identity_sent_for(context(), "support@test.local").await,
        Ok("b".into())
    );
    // Neither half of an address is compared case-sensitively.
    assert_eq!(
        identity_sent_for(context(), "Alice@TEST.local").await,
        Ok("c".into())
    );

    // An address no identity covers is refused before anything is created: sending it
    // under some other identity would break RFC 8621 §6's MUST on the client's side.
    let refused = identity_sent_for(context(), "stranger@example.test").await;
    assert!(refused.unwrap_err().contains("may send as"));
}

#[tokio::test]
async fn a_wildcard_identity_covers_its_domain_but_an_exact_one_wins() {
    // RFC 8621 §6: an identity whose address is `*@domain` may send as any address there.
    // Rewrite the captured group identity into one, keeping its place ahead of alice's.
    let mut context = fixture("submit_context_shared_identities_response.json");
    let identities = context["methodResponses"][1][1]["list"]
        .as_array_mut()
        .unwrap();
    assert_eq!(identities[0]["email"], "support@test.local");
    identities[0]["email"] = json!("*@test.local");

    assert_eq!(
        identity_sent_for(context.clone(), "carol@test.local").await,
        Ok("b".into())
    );
    // Alice's own identity names her exactly, so it wins over the wildcard listed first.
    assert_eq!(
        identity_sent_for(context.clone(), "alice@test.local").await,
        Ok("c".into())
    );
    // The wildcard is a domain match, not a suffix match.
    assert!(
        identity_sent_for(context, "carol@nottest.local")
            .await
            .is_err()
    );
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
