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

use std::collections::{BTreeSet, HashSet};

use engine_core::{
    ids::{MessageIdHeader, ProviderKey},
    mail::{EmailAddress, Keyword, Mailbox, MailboxRole},
};
use engine_provider::{Draft, ProviderResult, SubmissionReceipt};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    mail::mailbox_from_json,
    mutate::keyword_pointer,
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
///
/// The request that submits is the one whose failure can leave the send ambiguous: once it
/// may have reached the server, the message may have been sent, so a failure there needs
/// confirming rather than retrying ([`JmapError::into_submission_error`]). Everything before
/// it sends nothing and fails as it always does.
pub(crate) async fn send(
    executor: &dyn Executor,
    mail_account: &str,
    submission_account: &str,
    draft: &Draft,
) -> ProviderResult<SubmissionReceipt> {
    let context = resolve_context(executor, mail_account, submission_account).await?;
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

    let resp = (executor.submit(&req).await).map_err(JmapError::into_submission_error)?;
    let receipt = parse_receipt(
        resp.result(&email_set)?,
        resp.result(&submission_set)?,
        &draft.message_id,
    )?;
    let kept = tag_sent_copy(executor, mail_account, receipt.email_key.as_str(), draft).await;
    Ok(receipt.with_sent_copy_keywords(kept))
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
pub(crate) fn role_id(mailboxes: &[Mailbox], role: &MailboxRole) -> Result<String, JmapError> {
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

/// Builds the `Email/set` create object for the draft, referencing the uploaded
/// attachment `blob_ids` (one per `draft.attachments`, in order).
pub(crate) fn build_draft(drafts_mailbox: &str, draft: &Draft, blob_ids: &[String]) -> Value {
    let mut mailbox_ids = Map::new();
    mailbox_ids.insert(drafts_mailbox.to_owned(), Value::Bool(true));
    let (body_structure, body_values) = body(draft, blob_ids);
    let mut create = json!({
        "mailboxIds": mailbox_ids,
        "keywords": { "$draft": true, "$seen": true },
        "from": [address(&draft.from)],
        "to": draft.to.iter().map(address).collect::<Vec<_>>(),
        "subject": draft.subject,
        "messageId": [draft.message_id.as_str()],
        "bodyStructure": body_structure,
        "bodyValues": body_values,
    });
    // The Bcc header stays on the stored copy, as on every other transport's filed copy:
    // RFC 8621 §7.5 has the server remove it during delivery.
    for (property, addresses) in [("cc", &draft.cc), ("bcc", &draft.bcc)] {
        if !addresses.is_empty() {
            create[property] = addresses.iter().map(address).collect();
        }
    }
    if let Some(parent) = &draft.in_reply_to {
        create["inReplyTo"] = json!([parent.as_str()]);
    }
    if !draft.references.is_empty() {
        create["references"] = draft
            .references
            .iter()
            .map(MessageIdHeader::as_str)
            .collect();
    }
    create
}

/// Builds the `EmailSubmission/set` create object and the `onSuccessUpdateEmail`
/// patch that files the sent copy.
fn build_submission(context: &SubmitContext, draft: &Draft) -> (Value, Value) {
    let create = json!({
        "emailId": "#draft",
        "identityId": context.identity,
        "envelope": {
            "mailFrom": { "email": draft.from.email },
            "rcptTo": envelope_recipients(draft),
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

/// Every recipient of `draft`, To + Cc + Bcc, deduplicated case-insensitively.
///
/// Bcc is reached through the envelope alone once the server has removed its header, so a
/// submission that listed only To would never deliver to Cc or Bcc.
fn envelope_recipients(draft: &Draft) -> Vec<Value> {
    let mut seen = HashSet::new();
    draft
        .to
        .iter()
        .chain(&draft.cc)
        .chain(&draft.bcc)
        .filter(|recipient| seen.insert(recipient.email.to_ascii_lowercase()))
        .map(|recipient| json!({ "email": recipient.email }))
        .collect()
}

/// Sets `draft.sent_copy_keywords` on the filed copy `email_id`, returning the ones it
/// carries. Asks nothing, and costs no request, when the draft wants none.
///
/// A request of its own after the send, rather than part of the create or of
/// `onSuccessUpdateEmail`: the create is also a saved draft's, which must not carry them, and
/// a keyword the server refuses inside `onSuccessUpdateEmail` would fail the whole patch and
/// leave the sent copy in Drafts. Nor can it ride the send's request as an update of
/// `#draft`: Stalwart answers an update keyed by a creation id `notFound` (observed live).
/// The message has gone by now, so any failure here costs the keywords and nothing else.
async fn tag_sent_copy(
    executor: &dyn Executor,
    mail_account: &str,
    email_id: &str,
    draft: &Draft,
) -> BTreeSet<Keyword> {
    let Some(update) = keyword_update(mail_account, email_id, draft) else {
        return BTreeSet::new();
    };
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let call = req.invoke("Email/set", update);
    match executor.execute(&req).await {
        Ok(resp) => kept_keywords(resp.result(&call), draft),
        Err(_) => BTreeSet::new(),
    }
}

/// The `Email/set` that puts `draft.sent_copy_keywords` on `email_id`, or `None` when the
/// draft asks for none.
fn keyword_update(mail_account: &str, email_id: &str, draft: &Draft) -> Option<Value> {
    if draft.sent_copy_keywords.is_empty() {
        return None;
    }
    let patch: Map<String, Value> = draft
        .sent_copy_keywords
        .iter()
        .map(|keyword| (keyword_pointer(keyword.as_str()), Value::Bool(true)))
        .collect();
    let mut update = Map::new();
    update.insert(email_id.to_owned(), Value::Object(patch));
    Some(json!({ "accountId": mail_account, "update": update }))
}

/// The keywords the filed copy carries after [`keyword_update`]: every one when the server
/// applied the update, none when it refused it or answered without reporting it applied.
fn kept_keywords(result: Result<&Value, JmapError>, draft: &Draft) -> BTreeSet<Keyword> {
    let applied = result.ok().is_some_and(|result| {
        result
            .get("updated")
            .and_then(Value::as_object)
            .is_some_and(|updated| !updated.is_empty())
            && result
                .get("notUpdated")
                .and_then(Value::as_object)
                .is_none_or(Map::is_empty)
    });
    if applied {
        draft.sent_copy_keywords.clone()
    } else {
        BTreeSet::new()
    }
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
#[path = "submit_tests.rs"]
mod tests;
