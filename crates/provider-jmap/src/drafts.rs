//! Storing a draft on the server, and removing one: `Email/set` against the account's
//! Drafts mailbox.
//!
//! The object stored is exactly the one the send path creates before it submits
//! ([`build_draft`]), which is the whole difference between saving and sending on JMAP:
//! a draft is a submission that never got its `EmailSubmission/set`.
//!
//! **A replaced draft is created and destroyed in one method call.** RFC 8620 §5.3
//! fixes the order within a `Set` as create, then update, then destroy, so the new
//! message exists before the old one goes, and a caller cannot observe a window with
//! neither. That is a stronger promise than IMAP can make, where the two are separate
//! commands.
//!
//! **A draft's keywords are part of its create**, beside `$draft` and `$seen`. Every save
//! creates a new message, so each one carries exactly the keywords that save asked for.

use engine_core::{
    ids::ProviderKey,
    mail::{MailboxRole, MessageMetadata},
};
use engine_provider::{Draft, DraftReceipt};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    mail::mailbox_from_json,
    request::{Request, capability},
    submit::{build_draft, created_id, role_id, set_error},
    sync_ops::objects,
};

/// Stores `draft` in the account's Drafts mailbox carrying the keywords of `metadata`, destroying
/// the message named by `replacing`, and returns the new message's id and the keywords it carries:
/// all of them, since a create the server accepted holds every keyword it was given.
///
/// # Errors
///
/// [`JmapError`] if the Drafts mailbox cannot be resolved, an attachment upload fails,
/// or the server refused the create. A `destroy` the server refused is **not** an error:
/// the draft the caller asked for is stored either way, and failing here would have the
/// caller create a further copy on retry.
pub(crate) async fn put_draft(
    executor: &dyn Executor,
    mail_account: &str,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
    metadata: &MessageMetadata,
) -> Result<DraftReceipt, JmapError> {
    let drafts = drafts_mailbox(executor, mail_account).await?;
    // Attachment bytes first: the draft references each by the server-assigned `blobId`,
    // which cannot be known before the upload (RFC 8620 §6.1). A re-save reuses the blob
    // the stored draft already carries wherever the attachment is unchanged, so saving
    // again does not re-send a file the server has (`crate::drafts_blobs`).
    let blob_ids = crate::drafts_blobs::resolve(executor, mail_account, draft, replacing).await?;

    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let mut create = Map::new();
    let mut stored = build_draft(&drafts, draft, &blob_ids);
    for keyword in &metadata.keywords {
        stored["keywords"][keyword.as_str()] = Value::Bool(true);
    }
    create.insert("draft".to_owned(), stored);
    let mut arguments = json!({ "accountId": mail_account, "create": create });
    if let Some(superseded) = replacing {
        arguments["destroy"] = json!([superseded.as_str()]);
    }
    let set = req.invoke("Email/set", arguments);

    let resp = executor.execute(&req).await?;
    let result = resp.result(&set)?;
    let id = created_id(result, "draft").ok_or_else(|| set_error(result, "draft", "Email/set"))?;
    let key = ProviderKey::new(id)
        .map_err(|e| JmapError::protocol(format!("bad created email id: {e}")))?;
    // The server stamps a draft's arrival time itself, so only the keywords are kept.
    Ok(DraftReceipt::stored(key)
        .with_kept(MessageMetadata::default().with_keywords(metadata.keywords.clone())))
}

/// Destroys a stored draft, reporting one that is already gone as done.
///
/// # Errors
///
/// [`JmapError`] on a transport failure or a `SetError` other than `notFound`.
pub(crate) async fn delete_draft(
    executor: &dyn Executor,
    mail_account: &str,
    draft: &ProviderKey,
) -> Result<(), JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let set = req.invoke(
        "Email/set",
        json!({ "accountId": mail_account, "destroy": [draft.as_str()] }),
    );

    let resp = executor.execute(&req).await?;
    settled(resp.result(&set)?, draft)
}

/// Reads a `destroy`'s outcome, treating an absent message as the state asked for.
///
/// The op behind this is retryable, so the second call names a message the first one
/// destroyed; `notFound` is that call succeeding, and reporting it as a failure would
/// park the op forever.
fn settled(result: &Value, draft: &ProviderKey) -> Result<(), JmapError> {
    let destroyed = result
        .get("destroyed")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(draft.as_str())));
    if destroyed {
        return Ok(());
    }
    let error_type = result
        .get("notDestroyed")
        .and_then(|nd| nd.get(draft.as_str()))
        .and_then(|err| err.get("type"))
        .and_then(Value::as_str);
    match error_type {
        None | Some("notFound") => Ok(()),
        Some(other) => Err(JmapError::Method {
            call_id: "Email/set".to_owned(),
            error_type: other.to_owned(),
        }),
    }
}

/// The id of the account's Drafts mailbox.
async fn drafts_mailbox(executor: &dyn Executor, mail_account: &str) -> Result<String, JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let mailboxes = req.invoke("Mailbox/get", json!({ "accountId": mail_account }));
    let resp = executor.execute(&req).await?;
    let list = objects(resp.result(&mailboxes)?, mailbox_from_json)?;
    role_id(&list, &MailboxRole::Drafts)
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
mod tests;
