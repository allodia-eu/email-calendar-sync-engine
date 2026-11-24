//! Importing into a JMAP account: `Email/import` (RFC 8621 §4.8), the `import` feature
//! (`providers.md` → "Import").
//!
//! - **The bytes travel inline where they fit** (`plan`): RFC 9404 `Blob/upload` in the same
//!   request as the `Email/import` that references it, sized against `maxSizeRequest` in base64
//!   bytes. A message too large for that goes to the `uploadUrl` first, at most
//!   `maxConcurrentUpload` at once, and is referenced by its blob id. Every request goes through
//!   the client's one transport, so it waits inside the account's `RequestGate` like any other.
//! - **The limits are the session's**: `maxSizeUpload` is the largest message and
//!   `maxMailboxesPerEmail` the most mailboxes per message, and `MailImportLimits::check` refuses a
//!   message over either before anything is sent.
//! - **Each message is answered on its own** (`wire`): stored, already there (`alreadyExists`, a
//!   link), refused, or possibly stored when its answer was lost.
//! - **A request that fails whole ends the batch.** Its messages are answered by whether it may
//!   have reached the server, and every message after them as not sent, so the count still matches.
//!   A failure's detail names its class and code, never the server's text.
//! - **The import cursor is the account's `Email` state**, read in the request that checks the
//!   mailbox exists; a delta from it through `stream_email` lists what was stored after it, in any
//!   mailbox.
//! - **Mailboxes are not created here.** A host creates a folder with
//!   `MailboxWrites::edit_mailbox`, the one `Mailbox/set` path, and imports into its id.
//! - **Mail rules and the reply** (`rules`): `RulesImporter`, through the engine's one import over
//!   this adapter's own `MailRules` and `Vacation`, the folders the rules name asked of the server.

mod plan;
mod rules;
mod wire;

use std::num::{NonZeroU64, NonZeroUsize};

use async_trait::async_trait;
use engine_core::{
    error::FailureClass,
    ids::{AccountId, BlobId, MailboxId},
    sync::SyncState,
};
use engine_provider::{
    ProviderError, ProviderResult,
    import::{ImportedMessage, MailImportLimits, MailImporter, MessageImport, not_sent},
};
use futures_util::{StreamExt, stream};
use plan::{Budget, Route};
use serde_json::json;
use wire::{Blob, Entry};

use crate::{
    JmapProvider,
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
    session::Session,
    session_accounts::JmapAccountId,
};

#[async_trait]
impl MailImporter for JmapProvider {
    async fn mail_import_limits(&self, _account: &AccountId) -> ProviderResult<MailImportLimits> {
        Ok(limits_of(self.executor().session()))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState> {
        let executor = self.executor();
        let account = executor.session().mail_account_id().map_err(coded)?;
        cursor_of(executor, account, mailbox).await
    }

    async fn import_messages(
        &self,
        _account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        let executor = self.executor();
        match executor.session().mail_account_id() {
            Ok(account) => import(executor, account, batch).await,
            Err(err) => {
                let err = coded(err);
                batch.iter().map(|_| Err(copy(&err))).collect()
            }
        }
    }
}

/// Whether the account takes a message's bytes at all: by its upload URL, or inline through
/// the blob extension.
pub(crate) fn takes_blobs(session: &Session) -> bool {
    session.upload_url().is_some()
        || matches!(
            session.mail_limits().blob_upload,
            crate::session_limits::BlobUpload::Supported { .. }
        )
}

/// What the session states: `maxSizeUpload` (or, with no `uploadUrl`, the largest blob one
/// `Blob/upload` takes), and `maxMailboxesPerEmail`, where a missing limit is none.
fn limits_of(session: &Session) -> MailImportLimits {
    let mail = session.mail_limits();
    let limits = MailImportLimits::new(mail.max_mailboxes_per_email.unwrap_or(NonZeroUsize::MAX));
    let largest = if session.upload_url().is_some() {
        Some(session.limits().max_size_upload)
    } else {
        match mail.blob_upload {
            crate::session_limits::BlobUpload::Supported { max_size } => max_size,
            crate::session_limits::BlobUpload::Unsupported => None,
        }
    };
    match largest.and_then(NonZeroU64::new) {
        Some(max) => limits.with_max_message_size(max),
        None => limits,
    }
}

/// The account's `Email` state now, read beside a `Mailbox/get` that says `mailbox` exists.
async fn cursor_of(
    executor: &dyn Executor,
    account: &JmapAccountId,
    mailbox: &MailboxId,
) -> Result<SyncState, ProviderError> {
    let mut request = Request::new([capability::CORE, capability::MAIL]);
    let found = request.invoke(
        "Mailbox/get",
        json!({ "accountId": account, "ids": [mailbox.as_str()], "properties": ["id"] }),
    );
    let state = request.invoke(
        "Email/get",
        json!({ "accountId": account, "ids": [], "properties": ["id"] }),
    );
    let response = executor.execute(&request).await.map_err(coded)?;
    let listed = response.result(&found).map_err(coded)?["list"]
        .as_array()
        .is_some_and(|list| !list.is_empty());
    if !listed {
        return Err(ProviderError::refused_by_target(
            engine_provider::TargetRefusal::NoSuchContainer,
        ));
    }
    response.result(&state).map_err(coded)?["state"]
        .as_str()
        .map(SyncState::new)
        .ok_or_else(|| coded(JmapError::protocol("Email/get answered no state")))
}

/// Imports `batch` into `account`, answering each message in order.
async fn import(
    executor: &dyn Executor,
    account: &JmapAccountId,
    batch: &[MessageImport],
) -> Vec<ProviderResult<ImportedMessage>> {
    let session = executor.session();
    let limits = limits_of(session);
    let budget = Budget::new(
        &session.limits(),
        session.mail_limits().blob_upload,
        account.as_str().len(),
    );
    let mut answers: Vec<Option<ProviderResult<ImportedMessage>>> =
        batch.iter().map(|_| None).collect();

    // Refused before anything is sent: over a stated limit.
    let mut sendable = Vec::new();
    for (index, message) in batch.iter().enumerate() {
        match limits.check(message) {
            Ok(()) => sendable.push((
                index,
                budget.item(message.raw.len(), wire::entry_bytes(index, message)),
            )),
            Err(refused) => answers[index] = Some(Err(refused)),
        }
    }

    // The messages that cannot go inline go to the upload URL first.
    let by_url: Vec<usize> = sendable
        .iter()
        .filter(|(_, item)| item.route == Route::ByUrl)
        .map(|(index, _)| *index)
        .collect();
    let (mut uploaded, ended) = upload_all(executor, account, batch, &by_url).await;
    if let Some(ending) = ended {
        // A throttle or a refused credential reaches every message: no import is sent.
        for (index, _) in sendable {
            answers[index] = Some(Err(match uploaded.remove(&index) {
                Some(Err(failed)) => failed,
                _ => not_sent(&ending),
            }));
        }
        return answers
            .into_iter()
            .map(|answer| answer.expect("every message is answered"))
            .collect();
    }
    let mut entries = Vec::new();
    let mut items = Vec::new();
    for (index, item) in sendable {
        let blob = match item.route {
            Route::Inline => Blob::Inline,
            Route::ByUrl => match uploaded
                .remove(&index)
                .expect("every message by URL was uploaded or failed")
            {
                Ok(id) => Blob::Uploaded(id),
                Err(failed) => {
                    answers[index] = Some(Err(failed));
                    continue;
                }
            },
        };
        entries.push(Entry {
            index,
            message: &batch[index],
            blob,
        });
        items.push(item);
    }

    let mut ended: Option<ProviderError> = None;
    for group in budget.pack(&items) {
        let group: Vec<Entry<'_>> = group.into_iter().map(|at| entries[at].clone()).collect();
        if let Some(ending) = &ended {
            for entry in &group {
                answers[entry.index] = Some(Err(not_sent(ending)));
            }
            continue;
        }
        let built = wire::build(account, &group);
        match executor.execute(&built.request).await {
            Ok(response) => {
                let read = wire::read(&built, &response, &group);
                for (entry, answer) in group.iter().zip(read.answers) {
                    answers[entry.index] = Some(answer);
                }
                ended = read.ending;
            }
            Err(failed) => {
                let failed = failure(failed, Stored::Possibly);
                for entry in &group {
                    answers[entry.index] = Some(Err(copy(&failed)));
                }
                ended = Some(failed);
            }
        }
    }
    answers
        .into_iter()
        .map(|answer| answer.expect("every message is answered"))
        .collect()
}

/// The blob ids of the messages at `indexes`, uploaded to the account's `uploadUrl` at most
/// `maxConcurrentUpload` at once, each answered with its id or why it was not stored; and the
/// failure that stopped the uploads, where one reaches every message (a throttle, a refused
/// credential), after which nothing more is uploaded and the messages not yet answered are
/// left out.
async fn upload_all(
    executor: &dyn Executor,
    account: &JmapAccountId,
    batch: &[MessageImport],
    indexes: &[usize],
) -> (Uploads, Option<ProviderError>) {
    let session = executor.session();
    let Some(template) = session.upload_url() else {
        let refused = indexes
            .iter()
            .map(|&index| {
                let refused = ProviderError::item_unavailable(
                    "the message does not fit a request and the target names no upload URL",
                );
                (index, Err(refused))
            })
            .collect();
        return (refused, None);
    };
    let url = crate::blob::upload_url(template, account);
    let width = session.limits().max_concurrent_upload.max(1);
    let mut uploads = stream::iter(indexes.iter().copied())
        .map(|index| {
            let url = &url;
            async move {
                let raw = batch[index].raw.as_bytes();
                let uploaded = executor
                    .upload(url, wire::MESSAGE, raw)
                    .await
                    .map_err(|err| failure(err, Stored::Never));
                (index, uploaded)
            }
        })
        .buffered(width);
    let mut answered = Uploads::new();
    while let Some((index, uploaded)) = uploads.next().await {
        let ending = uploaded.as_ref().err().filter(|failed| {
            matches!(
                failed.class(),
                FailureClass::RateLimited | FailureClass::Authentication
            )
        });
        let ending = ending.map(copy);
        answered.insert(index, uploaded);
        if ending.is_some() {
            return (answered, ending);
        }
    }
    (answered, None)
}

/// Each uploaded message's blob id, or why it was not stored.
type Uploads = std::collections::HashMap<usize, ProviderResult<BlobId>>;

/// Whether a failed exchange can have stored a message.
#[derive(Clone, Copy)]
enum Stored {
    /// An upload: what it stores is a blob, never a message.
    Never,
    /// An `Email/import`: where it may have reached the server, the message may be stored.
    Possibly,
}

/// A failed exchange, as the failure of each message it carried: classified as the adapter
/// classifies it, and its detail naming the failure's code rather than repeating the server's
/// text.
fn failure(err: JmapError, stored: Stored) -> ProviderError {
    let err = match stored {
        Stored::Never => err,
        Stored::Possibly => err.after_write(),
    };
    let code = code(&err);
    let classified = ProviderError::from(err);
    if classified.requires_confirmation() {
        ProviderError::needs_confirmation(format!("{code}: the message may be stored"))
    } else {
        with_detail(&classified, format!("not stored: {code}"))
    }
}

/// A failure that stored nothing (a read, a session without the account), classified as the
/// adapter classifies it, its detail its code.
fn coded(err: JmapError) -> ProviderError {
    let code = code(&err);
    with_detail(&ProviderError::from(err), code)
}

/// `classified`'s class and wait, under `detail`.
fn with_detail(classified: &ProviderError, detail: String) -> ProviderError {
    if classified.class() == FailureClass::RateLimited {
        ProviderError::rate_limited(detail, classified.retry_after())
    } else {
        ProviderError::new(classified.class(), detail)
    }
}

/// The code of an exchange's failure: its status or its class, never the server's prose. (A
/// method error is not an exchange's: it is read from the answer, in `wire`.)
fn code(err: &JmapError) -> String {
    match err {
        JmapError::Status { status, .. } => format!("HTTP {status}"),
        JmapError::Unconfirmed(inner) => code(inner),
        JmapError::Transport(_) => "the exchange failed".to_owned(),
        other => format!("{:?}", other.failure_class()),
    }
}

/// `err` again, for another message it answers: the failure of a whole request or of the
/// session, which is never about one message.
fn copy(err: &ProviderError) -> ProviderError {
    if err.requires_confirmation() {
        ProviderError::needs_confirmation(err.detail())
    } else if err.class() == FailureClass::RateLimited {
        ProviderError::rate_limited(err.detail(), err.retry_after())
    } else {
        ProviderError::new(err.class(), err.detail())
    }
}

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod scripted;
#[cfg(test)]
mod tests;
