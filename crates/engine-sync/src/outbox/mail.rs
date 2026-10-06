//! Mail writes: mutation ([`edit_mail`], or [`queue_mail_edit`] then [`send_mail_edit`]) and
//! reporting a message as junk / not junk / phishing ([`report_message`]). Submission is
//! `submit`'s.
//!
//! Both follow the shared outbox discipline (see the module docs): durable op → claim →
//! provider call → record the outcome under the lease.

use core::time::Duration;

use engine_core::{
    ids::{AccountId, ProviderKey},
    write::{
        IdempotencyKey, KeywordEdit, PendingOp, PendingOpId, PendingOpKind, PendingOutcome,
        ResourceKey,
    },
};
use engine_provider::{MailEdit, MessageReport, Provider};
use engine_store::{ClaimRejection, PendingOpState, Store, StoreRead, WorkerId};

use super::{claim_waiting, enqueue_and_claim, holding, record_failure};
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

/// What became of an attempt to send a queued mail edit ([`send_mail_edit`]).
///
/// Distinguishes the two answers a caller acts on differently: the server has it, or it is
/// still coming. A refusal for good is neither, and is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailEditSent {
    /// The server applied the edit.
    Applied(MailEditOutcome),
    /// Not this time, and still queued: the server refused for now (offline, rate limited, a
    /// timeout), another write to the same message held it, or an earlier failure's backoff has
    /// not elapsed. [`drain_outbox`](super::drain_outbox) sends it later; a keyword change stays
    /// shown meanwhile.
    Queued {
        /// The durable op that will be sent.
        op: PendingOpId,
        /// How long the server asked to be left alone, when it said.
        retry_after: Option<engine_core::time::Duration>,
    },
}

/// The durable op for `edit`, idempotent by `idempotency`.
///
/// `idempotency` must be **unique per edit intent**: the store dedups by `(account, key)` across
/// every op state, so a key derived only from the target would wrongly collapse two distinct
/// edits of one message (mark-read then mark-unread) into one op. The op's `resource_key` is the
/// target message, so edits to one message run one at a time. A keyword change carries its
/// [`KeywordEdit`], which the store shows from the moment the op is queued.
///
/// # Errors
///
/// Returns [`SyncError::Outbox`] if the edit cannot be encoded.
pub fn mail_edit_op(idempotency: &str, edit: &MailEdit) -> Result<PendingOp, SyncError> {
    let payload = serde_json::to_value(edit)
        .map_err(|e| SyncError::Outbox(format!("encode mail edit: {e}")))?;
    let idempotency_key =
        IdempotencyKey::new(idempotency).map_err(|e| SyncError::Outbox(e.to_string()))?;
    let resource = ResourceKey::new(format!("mail:{}", edit.target().as_str()))
        .map_err(|e| SyncError::Outbox(e.to_string()))?;
    let op = PendingOp::new(idempotency_key, PendingOpKind::MailEdit, resource, payload);
    Ok(match edit {
        MailEdit::SetKeywords {
            target,
            add,
            remove,
        } => op.with_keyword_edit(KeywordEdit::new(
            target.clone(),
            add.clone(),
            remove.clone(),
        )),
        MailEdit::MoveTo { .. } | MailEdit::Delete { .. } => op,
    })
}

/// Queues `edit` durably and returns its op, without reaching the server.
///
/// The first half of [`edit_mail`], for a caller that shows the edit before it is sent: a
/// keyword change is in the store, and in every read of it, when this returns. Needs no
/// connection, so an edit made offline is queued the same way and goes out with the next
/// [`drain_outbox`](super::drain_outbox). See [`mail_edit_op`] for `idempotency`.
///
/// # Errors
///
/// Returns [`SyncError::Outbox`] if the edit cannot be encoded, or [`SyncError::Store`] on a
/// store failure.
pub async fn queue_mail_edit<S: Store>(
    store: &S,
    account: &AccountId,
    idempotency: &str,
    edit: &MailEdit,
) -> Result<PendingOpId, SyncError> {
    let op = mail_edit_op(idempotency, edit)?;
    Ok(store.enqueue_pending_op(account.clone(), op).await?)
}

/// Sends the queued edit `op` (`edit`, as queued) now: claim → provider `edit_mail` → record.
///
/// The second half of [`edit_mail`]. A write to the same message already in flight is waited out
/// (up to the driver's resource bound), because opening a message and archiving it puts the second
/// write inside the first's round trip.
///
/// A failure the outbox will retry is not an error: the op parks and the answer is
/// [`MailEditSent::Queued`]. Only a failure that settles the op (one no retry can fix, or the
/// last of its attempts) is returned, after the store has taken back what it showed. There is no
/// `NeedsConfirmation` case: `UID STORE`/`MOVE`/`EXPUNGE` are not post-`DATA`-ambiguous (the
/// next sync reconciles the true state), and a stale-target `Conflict` is self-correcting after a
/// re-sync (`imap-smtp.md`).
///
/// # Errors
///
/// Returns [`SyncError::Provider`] if the server refused the edit for good, [`SyncError::Store`]
/// on a store failure, or [`SyncError::Outbox`] if the op is unknown or already settled.
pub async fn send_mail_edit<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    op: PendingOpId,
    edit: &MailEdit,
) -> Result<MailEditSent, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let leased = match claim_waiting(store, account, worker, ttl, op).await? {
        Ok(leased) => leased,
        Err(ClaimRejection::Busy | ClaimRejection::Backoff | ClaimRejection::DependencyUnmet) => {
            return Ok(MailEditSent::Queued {
                op,
                retry_after: None,
            });
        }
        Err(reason @ (ClaimRejection::Unknown | ClaimRejection::Settled)) => {
            return Err(SyncError::Outbox(format!(
                "queued op {op:?} cannot be sent: {reason:?}"
            )));
        }
    };

    match holding(store, &leased.lease, ttl, provider.edit_mail(account, edit)).await {
        Ok(receipt) => {
            store
                .mark_pending_op(
                    &leased.lease,
                    PendingOutcome::Succeeded {
                        provider_key: receipt.message_key.clone(),
                    },
                )
                .await?;
            Ok(MailEditSent::Applied(MailEditOutcome {
                op: leased.id,
                message_key: receipt.message_key,
            }))
        }
        Err(err) => {
            record_failure(store, &leased, &err).await?;
            // Parked or settled is the store's call (an attempt bound settles a retryable class
            // too), so ask it rather than reading the class here.
            if store.pending_op_state(op).await? == Some(PendingOpState::Pending) {
                Ok(MailEditSent::Queued {
                    op,
                    retry_after: err.retry_after(),
                })
            } else {
                Err(SyncError::Provider(err))
            }
        }
    }
}

/// Applies a [`MailEdit`] through the outbox in one call: [`queue_mail_edit`], then
/// [`send_mail_edit`]. The mail counterpart of
/// [`patch_calendar_event`](super::patch_calendar_event).
///
/// # Errors
///
/// As [`queue_mail_edit`] and [`send_mail_edit`].
#[allow(clippy::too_many_arguments)] // the provider, store, lease and edit an attempt needs
pub async fn edit_mail<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    idempotency: &str,
    edit: &MailEdit,
) -> Result<MailEditSent, SyncError>
where
    P: Provider,
    S: Store + StoreRead,
{
    let op = queue_mail_edit(store, account, idempotency, edit).await?;
    send_mail_edit(provider, store, account, worker, ttl, op, edit).await
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
