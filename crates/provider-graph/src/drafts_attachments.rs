//! Bringing a stored draft's attachment collection in line with the draft being saved.
//!
//! An attachment is a resource of its own on Graph (`POST` and `DELETE` under
//! `/messages/{id}/attachments`), so changing one does **not** mean replacing the
//! message: the draft keeps its id, its `internetMessageId` and its place in the folder,
//! and only what changed is sent.
//!
//! **Unchanged attachments are left alone, which is the point.** A saved draft is saved
//! repeatedly, and re-uploading every attachment each time would make a draft with a
//! large file expensive to keep. The listing asks for base properties only, so it does
//! not drag `contentBytes` down the wire either (Graph returns those by default).
//!
//! An attachment is matched by **name, media type and inline-ness**, which is what the
//! cheap listing can see. The residual: a file swapped for a *different* file of the same
//! name and media type reads as unchanged, and the stored draft keeps the earlier bytes.
//! Closing that would mean downloading every attachment on every save to compare, since
//! Graph's `size` is the encoded part size and not comparable to a local byte length
//! (measured: an 11-byte payload reports 191, a 1000-byte one 1192, the overhead growing
//! with the file name).

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, DraftAttachment};
use serde_json::{Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// What the cheap listing can see of a stored attachment.
struct Stored {
    id: String,
    identity: Identity,
}

/// How a stored attachment is matched to one being saved.
#[derive(PartialEq, Eq)]
struct Identity {
    name: String,
    content_type: String,
    inline: bool,
}

impl Identity {
    fn of(attachment: &DraftAttachment) -> Self {
        Self {
            name: attachment.file_name.clone(),
            content_type: attachment.media_type.clone(),
            inline: attachment.is_inline(),
        }
    }
}

/// Adds what `draft` has and the stored message does not, and removes what it no longer
/// carries. A no-op when the two already agree, which is the common case.
///
/// # Errors
///
/// A classified [`GraphError`] on a failed listing, upload or removal.
pub(crate) async fn reconcile(
    client: &GraphClient,
    message: &ProviderKey,
    draft: &Draft,
) -> Result<(), GraphError> {
    let mut stored = list(client, message).await?;
    let mut missing = Vec::new();

    // Consume a stored match per attachment, so two files sharing a name are handled by
    // count rather than one standing in for both.
    for attachment in &draft.attachments {
        let wanted = Identity::of(attachment);
        match stored.iter().position(|s| s.identity == wanted) {
            Some(at) => {
                stored.swap_remove(at);
            }
            None => missing.push(attachment),
        }
    }

    // Whatever no attachment claimed is no longer part of the draft.
    for leftover in stored {
        remove(client, message, &leftover.id).await?;
    }
    for attachment in missing {
        add(client, message, attachment).await?;
    }
    Ok(())
}

/// The stored collection, base properties only.
///
/// `contentId` is deliberately not asked for: it lives on `fileAttachment` rather than on
/// the base type, and naming it fails the whole request with a `BadRequest` rather than
/// being ignored.
async fn list(client: &GraphClient, message: &ProviderKey) -> Result<Vec<Stored>, GraphError> {
    let listed = client
        .get(&client.url(&format!(
            "/messages/{}/attachments?$select=id,name,contentType,size,isInline",
            message.as_str()
        )))
        .await?;
    Ok(listed
        .get("value")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(stored).collect())
        .unwrap_or_default())
}

fn stored(value: &Value) -> Option<Stored> {
    Some(Stored {
        id: value.get("id").and_then(Value::as_str)?.to_owned(),
        identity: Identity {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            content_type: value
                .get("contentType")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            inline: value
                .get("isInline")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    })
}

/// `POST /messages/{id}/attachments`.
///
/// The simple upload, which Graph caps at a few megabytes; a file past that is refused
/// with a classified error rather than silently dropped. The MIME create path this
/// complements has its own ceiling, so neither is the smaller limit in practice.
async fn add(
    client: &GraphClient,
    message: &ProviderKey,
    attachment: &DraftAttachment,
) -> Result<(), GraphError> {
    let mut body = json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": attachment.file_name,
        "contentType": attachment.media_type,
        "contentBytes": engine_rfc5322::base64_encode(&attachment.content),
        "isInline": attachment.is_inline(),
    });
    // An inline part is reached by `cid:` from the body, so it has to keep the id the
    // body references rather than the one Graph would mint.
    if let Some(content_id) = attachment.content_id() {
        body["contentId"] = json!(content_id.as_str());
    }
    client
        .post(
            &client.url(&format!("/messages/{}/attachments", message.as_str())),
            "application/json",
            serde_json::to_vec(&body)?,
        )
        .await?;
    Ok(())
}

/// `DELETE /messages/{id}/attachments/{id}`, treating one already gone as done.
async fn remove(
    client: &GraphClient,
    message: &ProviderKey,
    attachment: &str,
) -> Result<(), GraphError> {
    match client
        .delete(
            &client.url(&format!(
                "/messages/{}/attachments/{attachment}",
                message.as_str()
            )),
            None,
        )
        .await
    {
        Ok(()) | Err(GraphError::Status { status: 404, .. }) => Ok(()),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
#[path = "drafts_attachments_tests.rs"]
mod tests;
