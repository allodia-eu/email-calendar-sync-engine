//! One request of an import, `Blob/upload` then `Email/import` (RFC 9404 §4.1, RFC 8621 §4.8),
//! and what its answer says about each message.
//!
//! A message's creation ids are its position in the batch (`b<n>` for its blob, `m<n>` for its
//! email), so an answer is matched to the message whatever order the server lists it in.
//!
//! What an answer means for a message:
//!
//! - `created` → stored, with the metadata asked for kept: RFC 8621 §4.8 stores the keywords and
//!   `receivedAt` given, and the engine's arrival time has whole seconds, which is what Stalwart
//!   keeps (measured; `.123Z` is stored as `Z`).
//! - `alreadyExists` with its `existingId` → already there, a link rather than a failure.
//! - a refusal of the message itself (`invalidEmail`, `tooLarge`, `tooManyMailboxes`,
//!   `tooManyKeywords`) → `item_unavailable`;
//! - the target's own state → its named reason (`TargetRefusal`): `overQuota` is the account's
//!   storage, `invalidProperties` naming only `mailboxIds` a mailbox that does not exist,
//!   `forbidden` a mailbox the credential may not write. Any other refusal keeps its `SetError`
//!   class.
//! - its blob refused → that refusal, which is the cause even where the server then fails the whole
//!   `Email/import` with `invalidResultReference` (observed on Stalwart). The other messages of
//!   that call were not stored either, and are answered retryable.
//! - named nowhere, or a `serverPartialFail` → `requires_confirmation`: it may be stored.

use engine_core::{
    error::FailureClass,
    ids::{BlobId, ProviderKey},
};
use engine_provider::{
    ProviderError, ProviderResult, TargetRefusal,
    import::{ImportedMessage, MessageImport, not_sent},
};
use serde_json::{Map, Value, json};

use crate::{
    error::{JmapError, set_error_type},
    request::{CallId, Request, capability},
    response::Response,
    session_accounts::JmapAccountId,
};

/// The media type an imported message is uploaded as.
pub(crate) const MESSAGE: &str = "message/rfc822";

/// Where one message's bytes come from.
#[derive(Debug, Clone)]
pub(crate) enum Blob {
    /// From this request's `Blob/upload`.
    Inline,
    /// From an upload to the `uploadUrl`, which stored them under this id.
    Uploaded(BlobId),
}

/// One message of a request.
#[derive(Debug, Clone)]
pub(crate) struct Entry<'a> {
    /// Its position in the batch.
    pub(crate) index: usize,
    pub(crate) message: &'a MessageImport,
    pub(crate) blob: Blob,
}

/// A request ready to send, and the calls whose answers it is read from.
#[derive(Debug)]
pub(crate) struct Built {
    pub(crate) request: Request,
    upload: Option<CallId>,
    import: CallId,
}

fn blob_creation(index: usize) -> String {
    format!("b{index}")
}

fn email_creation(index: usize) -> String {
    format!("m{index}")
}

/// The `Email/import` entry of `message`, its bytes in the blob `blob_id`.
fn email_entry(message: &MessageImport, blob_id: &str) -> Value {
    let mailboxes: Map<String, Value> = message
        .mailboxes
        .iter()
        .map(|mailbox| (mailbox.as_str().to_owned(), Value::Bool(true)))
        .collect();
    let keywords: Map<String, Value> = message
        .metadata
        .keywords
        .iter()
        .map(|keyword| (keyword.as_str().to_owned(), Value::Bool(true)))
        .collect();
    let mut entry = json!({ "blobId": blob_id, "mailboxIds": mailboxes, "keywords": keywords });
    if let Some(at) = message.metadata.received_at {
        entry["receivedAt"] = json!(at);
    }
    entry
}

/// The bytes `message`'s `Email/import` entry takes on the wire without its blob id, which
/// depends on how the bytes travel (`plan`): its creation id and separators included.
pub(crate) fn entry_bytes(index: usize, message: &MessageImport) -> u64 {
    let entry = serde_json::to_vec(&email_entry(message, "")).map_or(0, |bytes| bytes.len());
    entry as u64 + email_creation(index).len() as u64 + 4
}

/// The request that imports `entries` into `account`.
pub(crate) fn build(account: &JmapAccountId, entries: &[Entry<'_>]) -> Built {
    let inline: Map<String, Value> = entries
        .iter()
        .filter(|entry| matches!(entry.blob, Blob::Inline))
        .map(|entry| {
            let data = engine_rfc5322::base64_encode(entry.message.raw.as_bytes());
            let blob = json!({ "data": [{ "data:asBase64": data }], "type": MESSAGE });
            (blob_creation(entry.index), blob)
        })
        .collect();
    let mut request = if inline.is_empty() {
        Request::new([capability::CORE, capability::MAIL])
    } else {
        Request::new([capability::CORE, capability::MAIL, capability::BLOB])
    };
    let upload = (!inline.is_empty()).then(|| {
        request.invoke(
            "Blob/upload",
            json!({ "accountId": account, "create": inline }),
        )
    });
    let emails: Map<String, Value> = entries
        .iter()
        .map(|entry| {
            let blob_id = match &entry.blob {
                Blob::Inline => format!("#{}", blob_creation(entry.index)),
                Blob::Uploaded(id) => id.as_str().to_owned(),
            };
            (
                email_creation(entry.index),
                email_entry(entry.message, &blob_id),
            )
        })
        .collect();
    let import = request.invoke(
        "Email/import",
        json!({ "accountId": account, "emails": emails }),
    );
    Built {
        request,
        upload,
        import,
    }
}

/// What an answer says: each message's answer, and the failure that ends the batch, if the
/// answer carries one.
#[derive(Debug)]
pub(crate) struct Read {
    /// The answer of each entry, in order.
    pub(crate) answers: Vec<ProviderResult<ImportedMessage>>,
    /// A throttle the answer met (an inline blob over the upload quota), which reaches every
    /// message after this request too.
    pub(crate) ending: Option<ProviderError>,
}

/// What `response` says about each of `entries`, in order.
pub(crate) fn read(built: &Built, response: &Response, entries: &[Entry<'_>]) -> Read {
    let uploads = built.upload.as_ref().map(|call| response.result(call));
    let blob_failure = |entry: &Entry<'_>| -> Option<ProviderError> {
        if !matches!(entry.blob, Blob::Inline) {
            return None;
        }
        let creation = blob_creation(entry.index);
        match uploads.as_ref()? {
            Ok(result) if result.pointer(&format!("/created/{creation}")).is_some() => None,
            Ok(result) => Some(match result.pointer(&format!("/notCreated/{creation}")) {
                Some(refusal) => blob_refusal(&creation, refusal),
                None => ProviderError::retryable(
                    "not stored: the upload answered nothing for the message",
                ),
            }),
            Err(err) => Some(method_failure(err, Call::Upload)),
        }
    };
    let failed = entries.iter().map(blob_failure).collect::<Vec<_>>();
    let any_blob_failed = failed.iter().any(Option::is_some);
    // The upload quota is a throttle on the whole account: it ends the batch.
    let ending = failed
        .iter()
        .flatten()
        .find(|blob| blob.class() == FailureClass::RateLimited)
        .map(copy_throttle);
    let import = response.result(&built.import);
    // Only a failure of the references themselves says the import stored nothing for them.
    let references_failed = matches!(
        &import,
        Err(JmapError::Method { error_type, .. }) if error_type == "invalidResultReference"
    );
    let answers = entries
        .iter()
        .zip(failed)
        .map(|(entry, blob)| {
            let creation = email_creation(entry.index);
            match &import {
                Ok(result) => {
                    if let Some(id) = result
                        .pointer(&format!("/created/{creation}/id"))
                        .and_then(Value::as_str)
                    {
                        return stored(id, entry.message);
                    }
                    if let Some(blob) = blob {
                        return Err(blob);
                    }
                    match result.pointer(&format!("/notCreated/{creation}")) {
                        Some(refusal) => import_refusal(&creation, refusal),
                        None => Err(ProviderError::needs_confirmation(
                            "the import answered nothing for the message: it may be stored",
                        )),
                    }
                }
                Err(err) => Err(match (blob, &ending) {
                    (Some(blob), _) => blob,
                    (None, Some(ending)) if references_failed => not_sent(ending),
                    (None, None) if references_failed && any_blob_failed => {
                        ProviderError::retryable(
                            "not stored: another message's upload in the same request was \
                             refused",
                        )
                    }
                    (None, _) => method_failure(err, Call::Import),
                }),
            }
        })
        .collect();
    Read { answers, ending }
}

/// A throttle again, for the batch it ends.
fn copy_throttle(throttle: &ProviderError) -> ProviderError {
    ProviderError::rate_limited(throttle.detail(), throttle.retry_after())
}

/// The answer for a message stored under `id`, keeping what it asked for.
fn stored(id: &str, message: &MessageImport) -> ProviderResult<ImportedMessage> {
    let key = ProviderKey::new(id)
        .map_err(|_| ProviderError::needs_confirmation("the import named an unusable id"))?;
    Ok(ImportedMessage::created(Some(key)).with_kept(message.metadata.clone()))
}

/// What a refused `Email/import` entry means.
fn import_refusal(creation: &str, refusal: &Value) -> ProviderResult<ImportedMessage> {
    match set_error_type(refusal) {
        // The server says the message is there; without its id the answer is to find it, not
        // to send it again (RFC 8621 §4.8 requires the id).
        "alreadyExists" => refusal
            .get("existingId")
            .and_then(Value::as_str)
            .and_then(|id| ProviderKey::new(id).ok())
            .map(ImportedMessage::existing)
            .ok_or_else(|| {
                ProviderError::needs_confirmation(
                    "the target holds the message and named no id for it",
                )
            }),
        kind @ ("invalidEmail" | "tooLarge" | "tooManyMailboxes" | "tooManyKeywords") => Err(
            ProviderError::item_unavailable(format!("the target refused the message ({kind})")),
        ),
        "blobNotFound" => Err(ProviderError::retryable(
            "not stored: the target no longer holds the uploaded message",
        )),
        // The target's own state (`providers.md`, "Import"), which waiting does not change.
        "overQuota" => Err(ProviderError::refused_by_target(TargetRefusal::StorageFull)),
        "invalidProperties" if names_only(refusal, "mailboxIds") => Err(
            ProviderError::refused_by_target(TargetRefusal::NoSuchContainer),
        ),
        "forbidden" => Err(ProviderError::refused_by_target(
            TargetRefusal::ReadOnlyContainer,
        )),
        _ => Err(JmapError::refused(creation, refusal).into()),
    }
}

/// What a refused inline blob means for its message.
fn blob_refusal(creation: &str, refusal: &Value) -> ProviderError {
    match set_error_type(refusal) {
        "tooLarge" => ProviderError::item_unavailable("the target refused the message (tooLarge)"),
        // The upload quota: the same wall as a `429` from the upload URL, measured on Stalwart.
        "overQuota" => {
            ProviderError::rate_limited("not stored: the target's upload quota is spent", None)
        }
        _ => JmapError::refused(creation, refusal).into(),
    }
}

/// Which call of the request a method error answered.
#[derive(Clone, Copy)]
enum Call {
    Upload,
    Import,
}

/// Whether a `SetError` names `property` and nothing else.
fn names_only(refusal: &Value, property: &str) -> bool {
    refusal
        .get("properties")
        .and_then(Value::as_array)
        .is_some_and(|names| {
            !names.is_empty() && names.iter().all(|name| name.as_str() == Some(property))
        })
}

/// A method error answering a whole call, as the failure of one of its messages: never stored,
/// except a `serverPartialFail`, after which it may be (RFC 8620 §3.6.2). An `overQuota` from
/// `Email/import` is the account's storage, its state; from `Blob/upload` it is the upload
/// quota, a throttle.
fn method_failure(err: &JmapError, call: Call) -> ProviderError {
    match err {
        JmapError::Method { error_type, .. } if error_type == "overQuota" => match call {
            Call::Upload => ProviderError::rate_limited(
                "not stored: the target's upload quota is spent (overQuota)",
                None,
            ),
            Call::Import => ProviderError::refused_by_target(TargetRefusal::StorageFull),
        },
        JmapError::Method {
            call_id,
            error_type,
        } => JmapError::Method {
            call_id: call_id.clone(),
            error_type: error_type.clone(),
        }
        .after_write()
        .into(),
        // A call the answer left out: it may have run.
        _ => ProviderError::needs_confirmation(
            "the answer left the import out: the message may be stored",
        ),
    }
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
