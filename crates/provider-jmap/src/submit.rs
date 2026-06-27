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

use engine_core::ids::{MessageIdHeader, ProviderKey};
use engine_core::mail::{EmailAddress, Mailbox, MailboxRole};
use engine_provider::{Draft, SubmissionReceipt};
use serde_json::{Map, Value, json};

use crate::error::JmapError;
use crate::mail::mailbox_from_json;
use crate::provider::Executor;
use crate::request::{Request, capability};
use crate::sync_ops::objects;

/// The server-assigned ids a submission needs as literals.
struct SubmitContext {
    drafts: String,
    sent: String,
    identity: String,
}

/// Sends `draft`: resolves context, then creates + submits + files it.
pub(crate) async fn send(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    draft: &Draft,
) -> Result<SubmissionReceipt, JmapError> {
    if !draft.attachments.is_empty() {
        return Err(JmapError::protocol(
            "JMAP submission does not yet support draft attachments",
        ));
    }

    let context = resolve_context(executor, mail_account, submission_account).await?;

    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mut email_create = Map::new();
    email_create.insert("draft".to_owned(), build_draft(&context, draft));
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

/// Reads the Drafts/Sent mailbox ids and the submission identity id in one request.
async fn resolve_context(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
) -> Result<SubmitContext, JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL, capability::SUBMISSION]);
    let mailboxes = req.invoke("Mailbox/get", json!({ "accountId": mail_account }));
    let identities = req.invoke("Identity/get", json!({ "accountId": submission_account }));
    let resp = executor.execute(&req).await?;

    let mailbox_list = objects(resp.result(&mailboxes)?, mailbox_from_json)?;
    Ok(SubmitContext {
        drafts: role_id(&mailbox_list, &MailboxRole::Drafts)?,
        sent: role_id(&mailbox_list, &MailboxRole::Sent)?,
        identity: first_identity(resp.result(&identities)?)?,
    })
}

/// Finds the id of the mailbox with `role`.
fn role_id(mailboxes: &[Mailbox], role: &MailboxRole) -> Result<String, JmapError> {
    mailboxes
        .iter()
        .find(|m| m.role.as_ref() == Some(role))
        .map(|m| m.id.as_str().to_owned())
        .ok_or_else(|| JmapError::session(format!("account has no {role} mailbox")))
}

/// The first identity id (the default From identity).
fn first_identity(result: &Value) -> Result<String, JmapError> {
    result
        .get("list")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|identity| identity.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| JmapError::session("account has no submission identity"))
}

/// Builds the `Email/set` create object for the draft.
fn build_draft(context: &SubmitContext, draft: &Draft) -> Value {
    let mut mailbox_ids = Map::new();
    mailbox_ids.insert(context.drafts.clone(), Value::Bool(true));
    let (body_structure, body_values) = body(draft);
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

/// Builds the JMAP body structure and values.
fn body(draft: &Draft) -> (Value, Value) {
    match &draft.html_body {
        Some(html) => (
            json!({
                "type": "multipart/alternative",
                "subParts": [
                    { "partId": "text", "type": "text/plain" },
                    { "partId": "html", "type": "text/html" },
                ],
            }),
            json!({
                "text": { "value": draft.text_body },
                "html": { "value": html },
            }),
        ),
        None => (
            json!({ "partId": "text", "type": "text/plain" }),
            json!({ "text": { "value": draft.text_body } }),
        ),
    }
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
    Ok(SubmissionReceipt::new(key, message_id.clone()))
}

/// The id of an object created under `creation_id`, if the create succeeded.
fn created_id<'a>(result: &'a Value, creation_id: &str) -> Option<&'a str> {
    result
        .get("created")
        .and_then(|created| created.get(creation_id))
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str)
}

/// Turns a `notCreated` `SetError` (RFC 8620 §5.3) into a classified method error.
fn set_error(result: &Value, creation_id: &str, method: &str) -> JmapError {
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
    use super::*;
    use engine_core::error::FailureClass;

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
        let create = build_draft(&context, &draft);
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

        let create = build_draft(&context, &draft);

        assert_eq!(create["bodyStructure"]["type"], "multipart/alternative");
        assert_eq!(create["bodyStructure"]["subParts"][0]["partId"], "text");
        assert_eq!(create["bodyStructure"]["subParts"][1]["partId"], "html");
        assert_eq!(create["bodyValues"]["text"]["value"], "Plain");
        assert_eq!(create["bodyValues"]["html"]["value"], "<p>Plain</p>");
    }
}
