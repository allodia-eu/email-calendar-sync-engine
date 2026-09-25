//! JMAP mail submission: `Email/set` draft creation + `EmailSubmission/set` with
//! `onSuccessUpdateEmail` (RFC 8621 §7).
//!
//! A send is the canonical two-step JMAP flow. First a "resolve context" request
//! reads the account's Drafts/Sent mailbox ids and submission identity (their ids
//! are server-assigned and cannot be templated into a `mailboxIds` map key, so
//! they must be known as literals first). Then one request creates the draft, then
//! submits it referencing the just-created email by creation id (`#draft`), and
//! files it via `onSuccessUpdateEmail` (move Drafts→Sent, clear `$draft`).
//!
//! This is only the provider side effect; durability and idempotency are the
//! caller's outbox (`engine-sync`). The pre-generated `Message-ID` is echoed in the
//! receipt so the sent copy reconciles when it syncs back (`store-and-sync.md`).

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Mailbox, MailboxRole},
};
use engine_provider::{Draft, SubmissionReceipt};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    mail::mailbox_from_json,
    request::{Request, capability},
    submit_body::body,
    sync_ops::objects,
};

/// The server-assigned ids a submission needs as literals.
struct SubmitContext {
    drafts: String,
    sent: String,
    identity: String,
}

/// Sends `draft`: resolves context, uploads any attachment blobs, then creates +
/// submits + files it.
pub(crate) async fn send(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    draft: &Draft,
) -> Result<SubmissionReceipt, JmapError> {
    let context = resolve_context(executor, mail_account, submission_account, &draft.from).await?;
    // Attachment bytes must be uploaded first: the draft references each by the
    // server-assigned `blobId` (RFC 8620 §6.1), which can only be known after upload.
    let blob_ids = upload_attachments(executor, mail_account, draft).await?;

    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mut email_create = Map::new();
    email_create.insert(
        "draft".to_owned(),
        build_draft(&context.drafts, draft, &blob_ids),
    );
    let email_set = req.invoke(
        "Email/set",
        json!({ "accountId": mail_account, "create": email_create }),
    );
    let (submission_create, on_success) = build_submission(&context, draft);
    let mut submission_map = Map::new();
    submission_map.insert("sub".to_owned(), submission_create);
    let submission_set = req.invoke(
        "EmailSubmission/set",
        json!({
            "accountId": submission_account,
            "create": submission_map,
            "onSuccessUpdateEmail": on_success,
        }),
    );

    let resp = executor.execute(&req).await?;
    parse_receipt(
        resp.result(&email_set)?,
        resp.result(&submission_set)?,
        &draft.message_id,
    )
}

/// Uploads every draft attachment's bytes, returning the `blobId`s in attachment
/// order (RFC 8620 §6.1). A no-op for an attachment-free draft.
///
/// # Errors
///
/// [`JmapError::Session`] if the draft has attachments but the server advertised no
/// `uploadUrl`, or the classified failure of an upload.
pub(crate) async fn upload_attachments(
    executor: &dyn Executor,
    mail_account: &str,
    draft: &Draft,
) -> Result<Vec<String>, JmapError> {
    if draft.attachments.is_empty() {
        return Ok(Vec::new());
    }
    let url = executor
        .session()
        .upload_url()
        .ok_or_else(|| JmapError::session("server advertised no uploadUrl; cannot attach"))?
        .replace("{accountId}", mail_account);
    let mut blob_ids = Vec::with_capacity(draft.attachments.len());
    for attachment in &draft.attachments {
        blob_ids.push(
            executor
                .upload(&url, &attachment.media_type, &attachment.content)
                .await?,
        );
    }
    Ok(blob_ids)
}

/// Reads the Drafts/Sent mailbox ids and the id of the identity that sends as `from`, in
/// one request.
async fn resolve_context(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    from: &EmailAddress,
) -> Result<SubmitContext, JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mailboxes = req.invoke("Mailbox/get", json!({ "accountId": mail_account }));
    let identities = req.invoke("Identity/get", json!({ "accountId": submission_account }));
    let resp = executor.execute(&req).await?;

    let mailbox_list = objects(resp.result(&mailboxes)?, mailbox_from_json)?;
    Ok(SubmitContext {
        drafts: role_id(&mailbox_list, &MailboxRole::Drafts)?,
        sent: role_id(&mailbox_list, &MailboxRole::Sent)?,
        identity: identity_for(resp.result(&identities)?, from)?,
    })
}

/// Finds the id of the mailbox with `role`.
pub(crate) fn role_id(mailboxes: &[Mailbox], role: &MailboxRole) -> Result<String, JmapError> {
    mailboxes
        .iter()
        .find(|m| m.role.as_ref() == Some(role))
        .map(|m| m.id.as_str().to_owned())
        .ok_or_else(|| JmapError::session(format!("account has no {role} mailbox")))
}

/// The id of the identity that sends as `from` (RFC 8621 §6).
///
/// **Not the first identity.** An account holds one identity per address it may send as,
/// and a user who belongs to a shared mailbox gets that mailbox's too — which Stalwart lists
/// *ahead of* the user's own. Taking the first submitted `From: alice@` under the
/// `support@` identity, and the server refused it with `forbiddenFrom`.
///
/// An identity's `email` is the address the client MUST put in `From` when sending as it,
/// except that a `*@domain` identity covers every address in that domain. So the exact
/// address wins, then a wildcard covering it, and neither half of the comparison is
/// case-sensitive in practice (RFC 5321 §2.3.11 for the domain). A `From` no identity
/// covers is refused here, before anything is created: sending it under some other
/// identity would be the client breaking the RFC's MUST, not the server's decision.
fn identity_for(result: &Value, from: &EmailAddress) -> Result<String, JmapError> {
    let identities: Vec<(&str, &str)> = result
        .get("list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|identity| {
            Some((
                identity.get("id").and_then(Value::as_str)?,
                identity.get("email").and_then(Value::as_str)?,
            ))
        })
        .collect();
    if identities.is_empty() {
        return Err(JmapError::session("account has no submission identity"));
    }
    let from_domain = from.email.rsplit_once('@').map(|(_, domain)| domain);
    identities
        .iter()
        .find(|(_, email)| email.eq_ignore_ascii_case(&from.email))
        .or_else(|| {
            identities.iter().find(|(_, email)| {
                email.strip_prefix("*@").is_some_and(|domain| {
                    from_domain.is_some_and(|own| own.eq_ignore_ascii_case(domain))
                })
            })
        })
        .map(|(id, _)| (*id).to_owned())
        .ok_or_else(|| {
            JmapError::Refused(format!(
                "no identity on this account may send as {:?}",
                from.email
            ))
        })
}

/// Builds the `Email/set` create object for the draft, referencing the uploaded
/// attachment `blob_ids` (one per `draft.attachments`, in order).
pub(crate) fn build_draft(drafts_mailbox: &str, draft: &Draft, blob_ids: &[String]) -> Value {
    let mut mailbox_ids = Map::new();
    mailbox_ids.insert(drafts_mailbox.to_owned(), Value::Bool(true));
    let (body_structure, body_values) = body(draft, blob_ids);
    json!({
        "mailboxIds": mailbox_ids,
        "keywords": { "$draft": true, "$seen": true },
        "from": [address(&draft.from)],
        "to": draft.to.iter().map(address).collect::<Vec<_>>(),
        "subject": draft.subject,
        "messageId": [draft.message_id.as_str()],
        "bodyStructure": body_structure,
        "bodyValues": body_values,
    })
}

/// Builds the `EmailSubmission/set` create object and the `onSuccessUpdateEmail`
/// patch that files the sent copy.
fn build_submission(context: &SubmitContext, draft: &Draft) -> (Value, Value) {
    let create = json!({
        "emailId": "#draft",
        "identityId": context.identity,
        "envelope": {
            "mailFrom": { "email": draft.from.email },
            "rcptTo": draft.to.iter().map(|a| json!({ "email": a.email })).collect::<Vec<_>>(),
        },
    });
    let mut patch = Map::new();
    patch.insert(format!("mailboxIds/{}", context.drafts), Value::Null);
    patch.insert(format!("mailboxIds/{}", context.sent), Value::Bool(true));
    patch.insert("keywords/$draft".to_owned(), Value::Null);
    let mut on_success = Map::new();
    on_success.insert("#sub".to_owned(), Value::Object(patch));
    (create, Value::Object(on_success))
}

/// A JMAP `EmailAddress` object, omitting a null display name.
fn address(addr: &EmailAddress) -> Value {
    match &addr.name {
        Some(name) => json!({ "name": name, "email": addr.email }),
        None => json!({ "email": addr.email }),
    }
}

/// Extracts the sent email's key, mapping a `SetError` on either create into a
/// classified [`JmapError`].
fn parse_receipt(
    email_result: &Value,
    submission_result: &Value,
    message_id: &MessageIdHeader,
) -> Result<SubmissionReceipt, JmapError> {
    let email_id = created_id(email_result, "draft")
        .ok_or_else(|| set_error(email_result, "draft", "Email/set"))?;
    if created_id(submission_result, "sub").is_none() {
        return Err(set_error(submission_result, "sub", "EmailSubmission/set"));
    }
    let key = ProviderKey::new(email_id)
        .map_err(|e| JmapError::protocol(format!("bad created email id: {e}")))?;
    // Filing is the server's own `onSuccessUpdateEmail`, in the same request that submitted
    // — so a submission that succeeded filed the copy. The one shape this does not cover:
    // the implicit `Email/set` can report the move `notUpdated` on its own, and the copy
    // then stays in Drafts. That response is a third entry sharing the submission's call id
    // and is not read here, so it would pass as `Filed`. Unlike a lost IMAP `APPEND` the
    // message is still in the account and still syncs, which is why it has not forced the
    // extra plumbing.
    Ok(SubmissionReceipt::filed(key, message_id.clone()))
}

/// The id of an object created under `creation_id`, if the create succeeded.
pub(crate) fn created_id<'a>(result: &'a Value, creation_id: &str) -> Option<&'a str> {
    result
        .get("created")
        .and_then(|created| created.get(creation_id))
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
}

/// Turns a `notCreated` `SetError` (RFC 8620 §5.3) into a classified method error.
pub(crate) fn set_error(result: &Value, creation_id: &str, method: &str) -> JmapError {
    let error_type = result
        .get("notCreated")
        .and_then(|nc| nc.get(creation_id))
        .and_then(|err| err.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    JmapError::Method {
        call_id: method.to_owned(),
        error_type,
    }
}

#[cfg(test)]
mod tests {
    use engine_core::error::FailureClass;

    use super::*;

    fn send_response() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/submit_send_response.json")).unwrap()
    }

    fn results(doc: &Value) -> (Value, Value) {
        // methodResponses: [Email/set "0", EmailSubmission/set "1", implicit Email/set "1"]
        let responses = doc["methodResponses"].as_array().unwrap();
        (responses[0][1].clone(), responses[1][1].clone())
    }

    fn message_id() -> MessageIdHeader {
        MessageIdHeader::new("step4-send-probe-0002@test.local").unwrap()
    }

    #[test]
    fn parses_the_sent_email_key_and_echoes_message_id() {
        let doc = send_response();
        let (email, submission) = results(&doc);
        let receipt = parse_receipt(&email, &submission, &message_id()).unwrap();
        // The created email id (kept across the Drafts→Sent move) is the resolved key.
        assert_eq!(receipt.email_key.as_str(), "bmaaaaal");
        assert_eq!(receipt.message_id, message_id());
    }

    #[test]
    fn email_set_error_classifies_and_aborts() {
        let email = json!({
            "notCreated": { "draft": { "type": "invalidProperties", "properties": ["from"] } }
        });
        let submission = json!({ "created": { "sub": { "id": "x" } } });
        let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[test]
    fn submission_error_classifies_after_email_created() {
        // The observed Stalwart failure when identityId is missing.
        let email = json!({ "created": { "draft": { "id": "e1" } } });
        let submission = json!({
            "notCreated": { "sub": { "type": "invalidProperties", "properties": ["identityId"] } }
        });
        let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Permanent);
    }

    #[test]
    fn rate_limited_submission_is_retryable() {
        let email = json!({ "created": { "draft": { "id": "e1" } } });
        let submission = json!({ "notCreated": { "sub": { "type": "rateLimit" } } });
        let err = parse_receipt(&email, &submission, &message_id()).unwrap_err();
        assert!(err.failure_class().is_retryable());
    }

    #[test]
    fn build_draft_targets_drafts_and_carries_message_id() {
        let context = SubmitContext {
            drafts: "d".to_owned(),
            sent: "e".to_owned(),
            identity: "b".to_owned(),
        };
        let draft = Draft::new(
            message_id(),
            EmailAddress::named("Alice", "alice@test.local"),
            vec![EmailAddress::new("bob@test.local")],
            "Subject",
            "Body",
        );
        let create = build_draft(&context.drafts, &draft, &[]);
        assert_eq!(create["mailboxIds"]["d"], json!(true));
        assert_eq!(create["keywords"]["$draft"], json!(true));
        assert_eq!(create["messageId"][0], "step4-send-probe-0002@test.local");
        assert_eq!(create["from"][0]["email"], "alice@test.local");

        let (submission, on_success) = build_submission(&context, &draft);
        assert_eq!(submission["emailId"], "#draft");
        assert_eq!(submission["identityId"], "b");
        // onSuccessUpdateEmail moves Drafts→Sent and clears $draft.
        assert_eq!(on_success["#sub"]["mailboxIds/d"], Value::Null);
        assert_eq!(on_success["#sub"]["mailboxIds/e"], json!(true));
        assert_eq!(on_success["#sub"]["keywords/$draft"], Value::Null);
    }

    #[test]
    fn build_draft_carries_html_as_alternative_body() {
        let context = SubmitContext {
            drafts: "d".to_owned(),
            sent: "e".to_owned(),
            identity: "b".to_owned(),
        };
        let draft = Draft::new(
            message_id(),
            EmailAddress::new("alice@test.local"),
            vec![EmailAddress::new("bob@test.local")],
            "Subject",
            "Plain",
        )
        .with_html_body("<p>Plain</p>");

        let create = build_draft(&context.drafts, &draft, &[]);

        assert_eq!(create["bodyStructure"]["type"], "multipart/alternative");
        assert_eq!(create["bodyStructure"]["subParts"][0]["partId"], "text");
        assert_eq!(create["bodyStructure"]["subParts"][1]["partId"], "html");
        assert_eq!(create["bodyValues"]["text"]["value"], "Plain");
        assert_eq!(create["bodyValues"]["html"]["value"], "<p>Plain</p>");
    }
}
