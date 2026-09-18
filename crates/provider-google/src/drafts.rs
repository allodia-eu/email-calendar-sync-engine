//! Storing a draft on the server, and removing one: Gmail's `users.drafts` collection.
//!
//! Gmail is the one transport here with a **draft object of its own**, so the key this
//! returns is a draft id, not a message id: `drafts.update` replaces a draft's content
//! and keeps that id, while the message underneath it is replaced and gets a new one.
//! Keying by the draft is what lets a re-save be one atomic call rather than a create
//! and a delete, which is what the other three adapters are reduced to.
//!
//! The key is opaque to a caller and is only ever handed back to these two verbs, so the
//! difference does not reach one.

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, ProviderResult};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::{base64url, error::GoogleError, transport::GoogleClient};

/// The `users.drafts` collection.
const DRAFTS: &str = "/gmail/v1/users/me/drafts";

/// Stores `draft`, replacing the content of the draft named by `replacing` when there is
/// one, and returns the draft's id.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on an assembly failure
/// or a rejected request.
pub(crate) async fn put_draft(
    client: &GoogleClient,
    draft: &Draft,
    replacing: Option<&ProviderKey>,
) -> ProviderResult<ProviderKey> {
    // The stored draft keeps its `Bcc` header, so resuming it restores every recipient;
    // it is stored in the mailbox and never delivered from here.
    let mime = engine_rfc5322::assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    // Gmail's `raw` is URL-safe base64, unlike Graph's standard-base64 `sendMail`.
    let body = json!({ "message": { "raw": base64url::encode(&mime) } })
        .to_string()
        .into_bytes();

    let stored = match replacing {
        // One call: the draft keeps its id and the old content is gone when it returns,
        // so there is no window in which the account holds two.
        Some(existing) => {
            client
                .put(
                    &client.url(&format!("{DRAFTS}/{}", existing.as_str())),
                    "application/json",
                    body,
                )
                .await?
        }
        None => {
            client
                .post(&client.url(DRAFTS), "application/json", body)
                .await?
        }
    };
    Ok(stored_key(stored.as_ref())?)
}

/// Removes a stored draft, reporting one that is already gone as done.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError) on any failure other
/// than the draft being absent.
pub(crate) async fn delete_draft(client: &GoogleClient, draft: &ProviderKey) -> ProviderResult<()> {
    // The op behind this is retryable, so the second call names a draft the first one
    // deleted; reporting that as a failure would park the op forever.
    match client
        .delete(&client.url(&format!("{DRAFTS}/{}", draft.as_str())), None)
        .await
    {
        Err(GoogleError::Status { status: 404, .. }) | Ok(()) => Ok(()),
        Err(other) => Err(other.into()),
    }
}

/// The `id` of the stored draft, which is the draft's own id (see the module note).
fn stored_key(stored: Option<&Value>) -> Result<ProviderKey, GoogleError> {
    let id = stored
        .and_then(|s| s.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| GoogleError::protocol("draft store returned no draft id"))?;
    ProviderKey::new(id).map_err(|e| GoogleError::protocol(format!("bad stored draft id: {e}")))
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
mod tests;
