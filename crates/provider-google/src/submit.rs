//! Mail submission via `users.messages.send` in base64url MIME.
//!
//! Gmail's `messages.send` takes the whole RFC 5322 message as a base64url `raw` field.
//! The adapter assembles it through the one shared assembler (`engine-rfc5322`) — so
//! `In-Reply-To`/`References`, `Cc`/`Bcc`, an HTML alternative, and attachments all ride
//! the same path as every other provider — then base64url-encodes it (Gmail's `raw`
//! field is URL-safe, unlike Graph's standard-base64 `sendMail`).
//!
//! Unlike SMTP/Graph `sendMail` (which return no id), Gmail's `send` **returns the sent
//! message's id** in the response, so the receipt carries the real provider key
//! immediately — no reconcile-by-`Message-ID` round-trip is needed. This matters because
//! **Gmail rewrites the `Message-ID` on send** (a captured real-behavior finding — the
//! caller's `<…@example.test>` comes back as `<…@mail.gmail.com>`), so a
//! reconcile-by-`Message-ID` would not match anyway; the returned id is authoritative.
//! The filed-assembly variant keeps the `Bcc` header on the stored Sent copy.

use engine_core::ids::ProviderKey;
use engine_provider::{Draft, ProviderResult, SubmissionReceipt};
use time::OffsetDateTime;

use crate::{base64url, error::GoogleError, transport::GoogleClient};

/// Sends `draft`: assembles the RFC 5322 message, base64url-encodes it, and `POST`s it to
/// `messages.send`.
///
/// # Errors
///
/// A classified [`ProviderError`](engine_provider::ProviderError): an assembly failure (a
/// header value carrying CR/LF/NUL) is permanent; `401`/`429`/`5xx` classify as
/// auth/rate-limit/retryable.
pub(crate) async fn send(
    client: &GoogleClient,
    draft: &Draft,
) -> ProviderResult<SubmissionReceipt> {
    // The filed variant keeps the Bcc header on the Sent copy (Gmail strips it from the
    // delivered envelope), mirroring the Graph submission path.
    let mime = engine_rfc5322::assemble_filed_message(draft, OffsetDateTime::now_utc())?;
    let raw = base64url::encode(&mime);
    let body = serde_json::to_vec(&serde_json::json!({ "raw": raw })).map_err(GoogleError::from)?;
    let response = client
        .post(
            &client.url("/gmail/v1/users/me/messages/send"),
            "application/json",
            body,
        )
        .await?;
    let key = sent_key(response.as_ref(), draft)?;
    Ok(SubmissionReceipt::new(key, draft.message_id.clone()))
}

/// The sent copy's provider key: the `id` Gmail returns in the `send` response (Gmail,
/// unlike SMTP, assigns and reveals it immediately). Falls back to a `Message-ID`-derived
/// placeholder if the response somehow carried none.
fn sent_key(
    response: Option<&serde_json::Value>,
    draft: &Draft,
) -> Result<ProviderKey, GoogleError> {
    if let Some(id) = response
        .and_then(|r| r.get("id"))
        .and_then(serde_json::Value::as_str)
    {
        return ProviderKey::new(id)
            .map_err(|e| GoogleError::protocol(format!("bad sent id: {e}")));
    }
    ProviderKey::new(format!("sent:{}", draft.message_id.as_str()))
        .map_err(|e| GoogleError::protocol(format!("bad placement key: {e}")))
}

#[cfg(test)]
#[path = "submit_tests.rs"]
mod tests;
