//! Reusing the bytes the server already holds when a draft is saved again.
//!
//! An `Email` is immutable (RFC 8621 §4.6: only `keywords` and `mailboxIds` may be
//! updated, proven against Stalwart), so a re-save has to create a replacement. That does
//! **not** mean re-sending the attachments: the replacement can reference the same
//! `blobId` the stored draft carries, and the server keeps the bytes it already has.
//! Without this, saving a draft with a large file attached would cost that file's size
//! every single time.
//!
//! ⚠️ **The `blobId` to reference is the one on the stored `Email`, not the one the
//! upload returned.** The server re-blobs the bytes when it embeds them into a message,
//! so the two differ, and only the former is on the draft being replaced.
//!
//! An attachment is matched by name, media type and **size**, and size here is the
//! decoded content length (unlike Graph's, which is the encoded part size), so a file
//! swapped for one of a different length is correctly seen as changed.

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, DraftAttachment};
use serde_json::{Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
    submit::upload_attachments,
};

/// What the stored draft holds, and the blob to reference for it.
struct Stored {
    blob_id: String,
    name: String,
    media_type: String,
    size: u64,
}

impl Stored {
    fn matches(&self, attachment: &DraftAttachment) -> bool {
        self.name == attachment.file_name
            && self.media_type == attachment.media_type
            && self.size == attachment.content.len() as u64
    }
}

/// The `blobId` for each of `draft`'s attachments, in order: the one already on
/// `replacing` where the attachment is unchanged, else a fresh upload.
///
/// # Errors
///
/// [`JmapError`] if the stored draft could not be read or an upload failed.
pub(crate) async fn resolve(
    executor: &dyn Executor,
    mail_account: &str,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
) -> Result<Vec<String>, JmapError> {
    let Some(existing) = replacing.filter(|_| !draft.attachments.is_empty()) else {
        // A first save, or nothing to carry over: every byte has to go up anyway.
        return upload_attachments(executor, mail_account, draft).await;
    };

    let mut stored = stored_attachments(executor, mail_account, existing).await?;
    let url = upload_url(executor, mail_account)?;
    let mut blob_ids = Vec::with_capacity(draft.attachments.len());
    for attachment in &draft.attachments {
        // Consume a match, so two files sharing a name are carried over by count rather
        // than one standing in for both.
        if let Some(at) = stored.iter().position(|s| s.matches(attachment)) {
            blob_ids.push(stored.swap_remove(at).blob_id);
        } else {
            blob_ids.push(
                executor
                    .upload(&url, &attachment.media_type, &attachment.content)
                    .await?,
            );
        }
    }
    Ok(blob_ids)
}

/// The upload endpoint for this account.
fn upload_url(executor: &dyn Executor, mail_account: &str) -> Result<String, JmapError> {
    Ok(executor
        .session()
        .upload_url()
        .ok_or_else(|| JmapError::session("server advertised no uploadUrl; cannot attach"))?
        .replace("{accountId}", mail_account))
}

/// The attachments on the draft being replaced.
///
/// A failure to read them is **not** fatal: the save can still upload everything, which
/// is what it would have done anyway. Only a transport error propagates.
async fn stored_attachments(
    executor: &dyn Executor,
    mail_account: &str,
    existing: &ProviderKey,
) -> Result<Vec<Stored>, JmapError> {
    let mut req = Request::new([capability::CORE, capability::MAIL]);
    let get = req.invoke(
        "Email/get",
        json!({
            "accountId": mail_account,
            "ids": [existing.as_str()],
            "properties": ["attachments"],
        }),
    );
    let resp = executor.execute(&req).await?;
    Ok(resp
        .result(&get)?
        .get("list")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|email| email.get("attachments"))
        .and_then(Value::as_array)
        .map(|attachments| attachments.iter().filter_map(stored).collect())
        .unwrap_or_default())
}

fn stored(value: &Value) -> Option<Stored> {
    Some(Stored {
        blob_id: value.get("blobId").and_then(Value::as_str)?.to_owned(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        media_type: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        size: value
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    })
}

#[cfg(test)]
#[path = "drafts_blobs_tests.rs"]
mod tests;
