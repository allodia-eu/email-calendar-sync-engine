//! Storing a draft on the server, and removing one: `POST /me/messages` in MIME format,
//! and `POST /me/messages/{id}/permanentDelete`.
//!
//! Graph creates a draft from the same base64 MIME the send path hands `/sendMail`, so
//! one assembler serves both and a saved draft is byte-identical to what sending it
//! would have delivered.
//!
//! **A re-save rewrites the stored message in place** ([`crate::drafts_patch`]): the key
//! and the `internetMessageId` are unchanged, and the draft stays where it is in the
//! folder rather than jumping on every save. An attachment change is part of that:
//! attachments are resources of their own, so only the ones that changed are sent
//! ([`crate::drafts_attachments`]) and the message still keeps its id.
//!
//! The fallback creates the new message and purges the old, and is reached in two cases
//! only: a draft carrying an iTIP part, which no message-resource property expresses,
//! and a draft that has been deleted from another device, where there is nothing to
//! rewrite and nothing to purge. Then the key **moves**, which is why the caller keeps
//! whatever key comes back rather than assuming.

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, ProviderResult};
use time::OffsetDateTime;

use crate::{error::GraphError, transport::GraphClient};

/// Stores `draft` in the account's Drafts folder and returns its key: the key
/// `replacing` named when the stored message could be rewritten in place, else the new
/// message's id.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) if the store failed. A
/// failure to remove a superseded message is **not** one: the new draft is stored, and
/// reporting it would have the caller create a further copy on retry.
pub(crate) async fn put_draft(
    client: &GraphClient,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
) -> ProviderResult<ProviderKey> {
    if let Some(existing) = replacing
        && let Some(unchanged) =
            crate::drafts_patch::patch_in_place(client, draft, existing).await?
    {
        return Ok(unchanged);
    }

    // The stored draft keeps its `Bcc` header, so resuming it restores every recipient;
    // it is created in the mailbox and never delivered from here.
    let mime = engine_rfc5322::assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    let body = engine_rfc5322::base64_encode(&mime).into_bytes();
    // MIME format, exactly as `sendMail` takes it: `text/plain` is the **request's**
    // content type, declaring that the body is a base64 RFC 5322 message, and says
    // nothing about the message's own parts. An HTML draft is a `multipart/alternative`
    // inside that MIME, and Graph parses it and stores the draft as `body.contentType:
    // html` (live-verified).
    let created = client
        .post(&client.url("/messages"), "text/plain", body)
        .await?;

    let key = created_key(created.as_ref())?;
    if let Some(superseded) = replacing {
        // Best effort by design (see the module note).
        let _ = remove(client, superseded).await;
    }
    Ok(key)
}

/// Removes a stored draft, reporting one that is already gone as done.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on any failure other
/// than the message being absent.
pub(crate) async fn delete_draft(client: &GraphClient, draft: &ProviderKey) -> ProviderResult<()> {
    remove(client, draft).await?;
    Ok(())
}

/// `POST /me/messages/{id}/permanentDelete`, treating a message that is not there as
/// the state the caller asked for.
///
/// **A hard delete, matching what the other three adapters do.** `DELETE
/// /me/messages/{id}` would only move the message to Deleted Items, so a discarded
/// draft would stay recoverable on Graph and be gone everywhere else, and a host would
/// have to know which provider it was talking to in order to say what it had done.
///
/// **Graph says "gone" two different ways, and which one depends on the endpoint**
/// (both observed against a real mailbox). Retrying this call on a draft the previous
/// attempt purged answers `404 ErrorItemNotFound`, which is the shape this verb meets
/// in practice. The soft `DELETE /me/messages/{id}` answers `403
/// ErrorCannotDeleteObject` for a message it has already moved, and `mutate.rs` records
/// the same code from the purge path during the retention window. Both are accepted,
/// because the op behind this verb is retryable and either means the draft is not
/// there. The code is matched rather than the bare status, since a genuine permission
/// failure is also a `403` and must still surface.
///
/// This is deliberately **not** how [`crate::mutate`] treats the same `403` on
/// [`MailEdit::Delete`](engine_provider::MailEdit::Delete): there the caller asked for
/// irreversible removal of the user's mail, and a message still sitting in Purges has
/// not had that done to it, so the ambiguity is reported. Here the caller asked only
/// that the draft stop being in Drafts, which a previous attempt already achieved.
async fn remove(client: &GraphClient, draft: &ProviderKey) -> Result<(), GraphError> {
    match client
        .post(
            &client.url(&format!("/messages/{}/permanentDelete", draft.as_str())),
            "application/json",
            Vec::new(),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(err) if already_gone(&err) => Ok(()),
        Err(other) => Err(other),
    }
}

/// Whether a failed delete means the message is not there.
fn already_gone(err: &GraphError) -> bool {
    let GraphError::Status { status, code, .. } = err else {
        return false;
    };
    match status {
        404 => true,
        403 => code.as_deref() == Some("ErrorCannotDeleteObject"),
        _ => false,
    }
}

/// The `id` Graph assigned the created message.
fn created_key(created: Option<&serde_json::Value>) -> Result<ProviderKey, GraphError> {
    let id = created
        .and_then(|c| c.get("id"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| GraphError::protocol("draft create returned no message id"))?;
    ProviderKey::new(id).map_err(|e| GraphError::protocol(format!("bad created draft id: {e}")))
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
mod tests;
