//! Importing into an IMAP mailbox: `APPEND` with the message's flags, keywords and
//! `INTERNALDATE` (the `import` feature, `providers.md` → "Import").
//!
//! - **One mailbox per message.** IMAP files a message in exactly one mailbox, so the limits say
//!   one, and a message filed in more is refused before anything is sent.
//! - **Keywords a folder cannot keep are left out, and the receipt says so.** The four system flags
//!   go on every `APPEND`. A custom keyword goes only where a `SELECT` of the folder shows `\*` in
//!   `PERMANENTFLAGS`, by the rule a Sent copy's keywords follow (`place.rs`): a server without it
//!   may drop the keyword and still answer `OK`, which a receipt would misreport.
//! - **The import cursor is the folder's sync cursor**, read by the same `SELECT` a pass opens it
//!   with, so a delta from it through a provider bound to the folder lists what was stored after
//!   it.
//! - **A batch runs on one connection.** A failure that leaves it unusable ends the batch: the
//!   message in flight is answered by whether its command was whole when the connection went, and
//!   every message after it as not sent.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU64, NonZeroUsize},
};

use async_trait::async_trait;
use engine_core::{
    ids::{AccountId, MailboxId},
    mail::Keyword,
    sync::SyncState,
};
use engine_provider::{
    ProviderError, ProviderResult,
    import::{ImportedMessage, MailImportLimits, MailImporter, MessageImport, MessageMetadata},
};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    ImapProvider,
    cursor::{CURSOR_VERSION, MailboxCursor},
    error::{ImapError, opens_with_code},
    mail::{keyword_to_flag, message_key},
    sync::effective_uid_next,
    transport::Connection,
    transport_append::AppendFailure,
};

#[async_trait]
impl<S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> MailImporter for ImapProvider<S> {
    async fn mail_import_limits(&self, _account: &AccountId) -> ProviderResult<MailImportLimits> {
        let session = self.session().await?;
        Ok(limits_of(&session))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        mailbox: &MailboxId,
    ) -> ProviderResult<SyncState> {
        let mut session = self.session().await?;
        let cursor = cursor_of(&mut session, mailbox).await;
        Ok(session.settle(cursor)?)
    }

    async fn import_messages(
        &self,
        _account: &AccountId,
        batch: &[MessageImport],
    ) -> Vec<ProviderResult<ImportedMessage>> {
        if batch.is_empty() {
            return Vec::new();
        }
        let mut session = match self.session().await {
            Ok(session) => session,
            Err(err) => {
                return batch
                    .iter()
                    .map(|_| Err(ProviderError::new(err.class(), err.detail())))
                    .collect();
            }
        };
        let limits = limits_of(&session);
        let mut keeps_keywords = BTreeMap::new();
        let mut answers = Vec::with_capacity(batch.len());
        let mut broken = false;
        for message in batch {
            match import_one(&mut session, &limits, &mut keeps_keywords, message).await {
                Ok(answer) => answers.push(answer),
                Err(ending) => {
                    answers.push(Err(ending));
                    broken = true;
                    break;
                }
            }
        }
        // A connection that failed is never parked, even when the failure fell on the batch's
        // last message: the next borrower would inherit a dead socket.
        if broken {
            session.discard();
            answers.resize_with(batch.len(), || {
                Err(ProviderError::retryable(
                    "not sent: the batch's connection failed before this message",
                ))
            });
        }
        answers
    }
}

/// What the session's server states: one mailbox per message, and its `APPENDLIMIT`.
fn limits_of<S>(session: &Connection<S>) -> MailImportLimits {
    let limits = MailImportLimits::new(NonZeroUsize::MIN);
    match session.negotiated.append_limit().and_then(NonZeroU64::new) {
        Some(max) => limits.with_max_message_size(max),
        None => limits,
    }
}

/// `mailbox`'s sync cursor now, opened as a pass opens it: with `CONDSTORE` where the session
/// enabled QRESYNC, so the cursor carries the `HIGHESTMODSEQ` a QRESYNC delta starts from.
async fn cursor_of<S>(
    session: &mut Connection<S>,
    mailbox: &MailboxId,
) -> Result<SyncState, ImapError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let select = if session.qresync_enabled() {
        session.select_condstore(mailbox.as_str()).await?
    } else {
        session.select(mailbox.as_str()).await?
    };
    let uid_next = effective_uid_next(session, &select).await?;
    Ok(MailboxCursor {
        uid_validity: select.uid_validity,
        uid_next,
        highest_modseq: select.highest_modseq,
        backfill_low: None,
        reconciled: false,
        version: CURSOR_VERSION,
    }
    .encode())
}

/// Imports one message. `Ok` carries its answer, a refusal included; `Err` is the answer of a
/// failure that left the connection unusable, which ends the batch.
async fn import_one<S>(
    session: &mut Connection<S>,
    limits: &MailImportLimits,
    keeps_keywords: &mut BTreeMap<MailboxId, bool>,
    message: &MessageImport,
) -> Result<ProviderResult<ImportedMessage>, ProviderError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    if let Err(refused) = limits.check(message) {
        return Ok(Err(refused));
    }
    let mailbox = message
        .mailboxes
        .iter()
        .next()
        .expect("a membership is never empty");
    let (system, custom): (BTreeSet<&Keyword>, BTreeSet<&Keyword>) = message
        .metadata
        .keywords
        .iter()
        .partition(|keyword| keyword_to_flag(keyword).starts_with('\\'));
    let keep_custom = if custom.is_empty() {
        false
    } else if let Some(&keeps) = keeps_keywords.get(mailbox) {
        keeps
    } else {
        match session.select(mailbox.as_str()).await {
            Ok(selected) => {
                keeps_keywords.insert(mailbox.clone(), selected.permanent_flags_allow_new);
                selected.permanent_flags_allow_new
            }
            Err(refused @ (ImapError::No(_) | ImapError::Bad(_))) => {
                return Ok(Err(refused.into()));
            }
            Err(lost) => return Err(lost.into()),
        }
    };
    let kept: BTreeSet<Keyword> = system
        .into_iter()
        .chain(custom.into_iter().filter(|_| keep_custom))
        .cloned()
        .collect();
    let flags = kept
        .iter()
        .map(keyword_to_flag)
        .collect::<Vec<_>>()
        .join(" ");
    match session
        .append_dated(
            mailbox.as_str(),
            &flags,
            message.metadata.received_at,
            message.raw.as_bytes(),
        )
        .await
    {
        Ok(uid) => {
            let key = uid.map(|(validity, uid)| message_key(mailbox.as_str(), validity, uid));
            // `INTERNALDATE` is stored as sent, so the arrival time is kept whenever one was given.
            let mut stored = MessageMetadata::default().with_keywords(kept);
            if let Some(at) = message.metadata.received_at {
                stored = stored.with_received_at(at);
            }
            Ok(Ok(ImportedMessage::created(key).with_kept(stored)))
        }
        Err(AppendFailure::NotStored(ImapError::No(detail)))
            if opens_with_code(&detail, "TOOBIG") =>
        {
            Ok(Err(ProviderError::item_unavailable(
                "the target refused the message as too large",
            )))
        }
        Err(AppendFailure::NotStored(refused @ (ImapError::No(_) | ImapError::Bad(_)))) => {
            Ok(Err(refused.into()))
        }
        Err(AppendFailure::NotStored(lost)) => Err(lost.into()),
        Err(AppendFailure::Unanswered(lost)) => Err(ProviderError::needs_confirmation(
            "the message went out and no answer came back: it may be stored",
        )
        .with_source(lost)),
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
