//! Mail writes: mutation ([`edit_mail`]) and reporting a message as junk / not junk /
//! phishing ([`report_message`]). Submission is `submit`'s.
//!
//! Both follow the shared outbox discipline (see the module docs): durable op → claim →
//! provider call → record the outcome under the lease.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{MailEdit, MessageReport, Provider};
use engine_store::{Store, WorkerId};

use super::{enqueue_and_claim, record_failure};
use crate::SyncError;

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
