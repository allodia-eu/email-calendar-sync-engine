//! Rewriting a stored draft in place: `PATCH /me/messages/{id}`.
//!
//! The cheap path, and the one a repeated save takes. Graph makes a draft's `subject`,
//! `body` and recipient collections writable (they are read-only once the message is no
//! longer a draft), so a text edit is **one request**, the message keeps its id, and it
//! keeps its place and its `internetMessageId` in the Drafts folder. Creating a
//! replacement and purging the old one costs two requests, moves the key, and makes the
//! draft jump every time it is saved.
//!
//! **What `PATCH` cannot do is change the attachment collection**, which is a navigation
//! property of its own: a patched draft keeps exactly the attachments it had (verified
//! against a real mailbox). So this path is taken only when the draft being stored
//! carries none, and it steps aside when the response says the stored one still does.

use engine_core::{ids::ProviderKey, mail::EmailAddress};
use engine_provider::Draft;
use serde_json::{Value, json};

use crate::{error::GraphError, transport::GraphClient};

/// Rewrites the draft `existing` names to match `draft`, returning its (unchanged) key.
///
/// `Ok(None)` means this path does not apply and the caller should store a replacement
/// instead: either `draft` carries something `PATCH` cannot express, or the stored
/// message still holds attachments this version does not, or it is no longer there.
///
/// # Errors
///
/// A classified [`GraphError`] on any failure other than the draft having gone missing,
/// which is reported as `Ok(None)` so a save can recover by storing a fresh copy.
pub(crate) async fn patch_in_place(
    client: &GraphClient,
    draft: &Draft,
    existing: &ProviderKey,
) -> Result<Option<ProviderKey>, GraphError> {
    // An attachment or an iTIP part has to go through the MIME assembler; neither is
    // expressible as a message-resource property.
    if !draft.attachments.is_empty() || draft.calendar.is_some() {
        return Ok(None);
    }

    let patched = match client
        .patch(
            &client.url(&format!("/messages/{}", existing.as_str())),
            "application/json",
            None,
            serde_json::to_vec(&body(draft))?,
        )
        .await
    {
        Ok(patched) => patched,
        // Deleted from another device between saves: storing a fresh copy is a better
        // answer than failing the save on a draft the user still has open.
        Err(GraphError::Status { status: 404, .. }) => return Ok(None),
        Err(other) => return Err(other),
    };

    // `PATCH` left the attachment collection alone, so a draft that had attachments and
    // no longer should still has them. Only a replacement can express that.
    if patched
        .as_ref()
        .and_then(|m| m.get("hasAttachments"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Ok(None);
    }
    Ok(Some(existing.clone()))
}

/// The message-resource properties a saved draft rewrites.
fn body(draft: &Draft) -> Value {
    let (content_type, content) = draft.html_body.as_ref().map_or_else(
        || ("Text", draft.text_body.clone()),
        |html| ("HTML", html.clone()),
    );
    json!({
        "subject": draft.subject,
        "body": { "contentType": content_type, "content": content },
        "from": { "emailAddress": address(&draft.from) },
        "toRecipients": recipients(&draft.to),
        "ccRecipients": recipients(&draft.cc),
        // Sent empty rather than omitted: a recipient the user removed has to come off
        // the stored draft, and `PATCH` leaves out what it is not told about.
        "bccRecipients": recipients(&draft.bcc),
    })
}

fn recipients(addresses: &[EmailAddress]) -> Value {
    Value::Array(
        addresses
            .iter()
            .map(|a| json!({ "emailAddress": address(a) }))
            .collect(),
    )
}

fn address(addr: &EmailAddress) -> Value {
    match &addr.name {
        Some(name) => json!({ "address": addr.email, "name": name }),
        None => json!({ "address": addr.email }),
    }
}

#[cfg(test)]
#[path = "drafts_patch_tests.rs"]
mod tests;
