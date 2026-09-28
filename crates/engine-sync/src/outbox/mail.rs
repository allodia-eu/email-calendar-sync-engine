//! Mail writes: submission ([`submit_mail`]), mutation ([`edit_mail`]) and reporting a
//! message as junk / not junk / phishing ([`report_message`]).
//!
//! Both follow the shared outbox discipline (see the module docs): durable op → claim →
//! provider call → record the outcome under the lease.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::Keyword,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{Draft, MailEdit, MessageReport, Provider, SentCopy, SubmissionReceipt};
use engine_store::{Store, WorkerId};

use super::{enqueue_and_claim, record_failure};
use crate::SyncError;

/// The result of a successful submission through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// The durable op that recorded the send.
    pub op: PendingOpId,
    /// The provider key of the sent message (for reconciliation/threading).
    pub email_key: ProviderKey,
    /// The `Message-ID` that was sent.
    pub message_id: MessageIdHeader,
    /// What became of the sender's own copy.
    ///
    /// A delivered message whose copy was **not** filed still completes the op — the mail
    /// has gone, and re-sending it would be far worse than a missing copy — so the op state
    /// cannot carry this and the outcome is the only place the fact survives. A caller that
    /// ignores it drops it for good: nothing later can rediscover a copy that was never
    /// written to the server.
    pub sent_copy: SentCopy,
    /// Which of the draft's [`Draft::sent_copy_keywords`] the filed copy carries; see
    /// [`SubmissionReceipt::sent_copy_keywords`](engine_provider::SubmissionReceipt::sent_copy_keywords).
    ///
    /// Like [`Self::sent_copy`], only this outcome carries it: a send the outbox drains
    /// later reports nothing, and the copy's keywords are then read from the next sync.
    ///
    /// Empty on an adapter that keeps them after the send
    /// ([`Capabilities::sent_copy_keywords_deferred`](engine_provider::Capabilities::sent_copy_keywords_deferred)):
    /// the outbox has recorded an edit for them instead, and
    /// whether it landed is read from sync.
    pub sent_copy_keywords: BTreeSet<Keyword>,
}

/// Sends `draft` through the outbox: durable op → claim → provider submit → record.
///
/// On a provider failure the op is recorded `Failed` (with the failure class) and
/// the error is returned — never blindly retried here.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the send fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the draft
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn submit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    draft: &Draft,
) -> Result<SubmitOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    // Durable record first: the draft as a pending op, idempotent by Message-ID.
    let payload =
        serde_json::to_value(draft).map_err(|e| SyncError::Outbox(format!("encode draft: {e}")))?;
    let message_id = draft.message_id.as_str();
    let idempotency = IdempotencyKey::new(format!("submit:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("draft:{message_id}"))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency, PendingOpKind::MailSubmit, resource, payload),
    )
    .await?;

    // Provider side effect, then record the outcome under the lease.
    match provider.submit_email(account, draft).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.email_key.clone(),
                    },
                )
                .await?;
            record_deferred_keywords(provider, store, account, draft, &receipt).await;
            Ok(SubmitOutcome {
                op: leased.id,
                email_key: receipt.email_key,
                message_id: receipt.message_id,
                sent_copy: receipt.sent_copy,
                sent_copy_keywords: receipt.sent_copy_keywords,
            })
        }
        Err(err) => {
            // An ambiguous send (e.g. a lost post-DATA SMTP ack) is parked for
            // confirmation, never recorded as a plain retryable failure — so the
            // outbox does not blind-retry and risk a double-send (`providers.md`).
            let outcome = if err.requires_confirmation() {
                PendingOutcome::NeedsConfirmation {
                    detail: err.detail().to_owned(),
                }
            } else {
                PendingOutcome::Failed {
                    class: err.class(),
                    retry_after: err.retry_after(),
                }
            };
            store.mark_pending_op(&leased.lease, outcome).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// Records the edit that keeps `draft`'s sent-copy keywords, on an adapter that keeps them
/// after the send rather than within it
/// ([`Capabilities::sent_copy_keywords_deferred`](engine_provider::Capabilities::sent_copy_keywords_deferred)).
///
/// Enqueued, not claimed: the send's answer does not wait for the copy to appear, and the
/// drainer applies the edit, retrying on the store's schedule while the adapter says the copy
/// is not there yet. Idempotent by the `Message-ID`, so a send reported twice records it once.
/// A store failure here is not the send's: the message has gone and its op is settled, so it
/// only leaves the keywords off the copy, which sync shows.
pub(super) async fn record_deferred_keywords<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    draft: &Draft,
    receipt: &SubmissionReceipt,
) where
    P: Provider,
    S: Store,
{
    if !provider
        .connection_info()
        .capabilities
        .sent_copy_keywords_deferred()
        || !receipt.sent_copy.is_filed()
    {
        return;
    }
    let wanted: BTreeSet<Keyword> = draft
        .sent_copy_keywords
        .difference(&receipt.sent_copy_keywords)
        .cloned()
        .collect();
    if wanted.is_empty() {
        return;
    }
    let edit = MailEdit::SetKeywords {
        target: receipt.email_key.clone(),
        add: wanted,
        remove: BTreeSet::new(),
    };
    let (Ok(payload), Ok(idempotency), Ok(resource)) = (
        serde_json::to_value(&edit),
        IdempotencyKey::new(format!("sent-copy-keywords:{}", draft.message_id.as_str())),
        ResourceKey::new(format!("mail:{}", receipt.email_key.as_str())),
    ) else {
        return;
    };
    let op = PendingOp::new(idempotency, PendingOpKind::MailEdit, resource, payload);
    let _ = store.enqueue_pending_op(account.clone(), op).await;
}

/// The result of a successful mail edit through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEditOutcome {
    /// The durable op that recorded the edit.
    pub op: PendingOpId,
    /// The provider key the edit resolved to (the edited message; for a move, its
    /// source key — the next sync reconciles the destination copy).
    pub message_key: ProviderKey,
}

/// Applies a [`MailEdit`] through the outbox: durable op → claim → provider
/// `edit_mail` → record. The mail counterpart of
/// [`patch_calendar_event`](super::patch_calendar_event).
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent — it
/// must be **unique per edit intent** (the store dedups by `(account, key)` across
/// every op state, so a key derived only from the target would wrongly collapse two
/// distinct edits of one message — e.g. mark-read then mark-unread — into one op).
/// The op's `resource_key` is the target message key, so the store serializes edits
/// to one message: a second edit whose target is already in flight waits for the first
/// (up to the driver's resource bound) rather than being refused, because opening a
/// message and archiving it puts the second write inside the first's round trip. A
/// provider failure is recorded `Failed` (with its class) and returned — never blindly
/// retried here. Unlike an SMTP send there is no `NeedsConfirmation` case:
/// `UID STORE`/`MOVE`/`EXPUNGE` are not post-`DATA`-ambiguous
/// (a periodic snapshot reconciles the true state), and a stale-target `Conflict` is
/// self-correcting after a re-sync (`imap-smtp.md`).
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the edit fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn edit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    edit: &MailEdit,
) -> Result<MailEditOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(edit)
        .map_err(|e| SyncError::Outbox(format!("encode mail edit: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", edit.target().as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(idempotency_key, PendingOpKind::MailEdit, resource, payload),
    )
    .await?;

    match provider.edit_mail(account, edit).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(MailEditOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}

/// The result of a successful report through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOutcome {
    /// The durable op that recorded the report.
    pub op: PendingOpId,
    /// The provider key the report resolved to — the reported message. Where the move
    /// mints a new key (IMAP) this is the source key; the next sync of the destination
    /// reconciles the copy, exactly as for [`MailEditOutcome`].
    pub message_key: ProviderKey,
}

/// Reports `report.target` through the outbox: durable op → claim → provider
/// `report_message` → record. The reporting counterpart of [`edit_mail`].
///
/// `idempotency` is the caller-minted key that makes the enqueue idempotent, and must be
/// **unique per report intent** for the same reason it is on an edit: the store dedups by
/// `(account, key)` across every op state, so a key derived only from the target would
/// collapse "junk" and a later "not junk" on one message into a single op — which is
/// precisely the pair a user is most likely to perform in sequence, having reported the
/// wrong message.
///
/// The op's `resource_key` is the target message key, shared with [`edit_mail`], so a
/// report and an edit of the same message serialize against each other rather than racing.
///
/// There is no `NeedsConfirmation` case: a report sends no mail, every transport's report
/// is idempotent (re-reporting is accepted), and a stale-target `Conflict` is
/// self-correcting after a re-sync.
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the report fails (after recording it),
/// [`SyncError::Store`] on a store failure, or [`SyncError::Outbox`] if the request
/// cannot be encoded or the just-enqueued op is not claimable.
pub async fn report_message<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    report: &MessageReport,
) -> Result<ReportOutcome, SyncError>
where
    P: Provider,
    S: Store,
{
    let payload = serde_json::to_value(report)
        .map_err(|e| SyncError::Outbox(format!("encode message report: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", report.target.as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let leased = enqueue_and_claim(
        store,
        account,
        worker,
        ttl,
        PendingOp::new(
            idempotency_key,
            PendingOpKind::MailReport,
            resource,
            payload,
        ),
    )
    .await?;

    match provider.report_message(account, report).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(ReportOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            })
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            Err(SyncError::Provider(err))
        }
    }
}
