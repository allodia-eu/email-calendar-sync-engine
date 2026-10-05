//! Sending a message: the inline driver ([`submit_mail`]) and the one attempt it shares with
//! the drainer.
//!
//! A send is never lost and never delivered twice, whatever happens to the process during it
//! (`store-and-sync.md` → "A send survives its process"). Three things here make that so:
//!
//! - the op is durable before anything else, so a send the process never got to is still queued;
//! - the provider records the hand-over through the outbox immediately before the first byte that
//!   could deliver the message ([`StoreHandOver`]), so whoever recovers an interrupted attempt
//!   knows which side of that line it ended on;
//! - the lease is renewed for as long as the attempt runs ([`super::holding`]), so no other worker
//!   recovers an attempt that is merely slow.

use core::time::Duration;
use std::collections::BTreeSet;

use engine_core::{
    ids::{AccountId, MessageIdHeader, ProviderKey},
    mail::Keyword,
    write::{IdempotencyKey, PendingOp, PendingOpId, PendingOpKind, PendingOutcome, ResourceKey},
};
use engine_provider::{
    Draft, HandOver, HandOverLog, Provider, ProviderError, SentCopy, SubmissionReceipt,
};
use engine_store::{LeasedPendingOp, OpLease, Store, StoreError, WorkerId};

use super::{enqueue_and_claim, holding};
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
    pub sent_copy_keywords: BTreeSet<Keyword>,
}

/// The outbox's record of a hand-over: [`Store::record_hand_over`] under the attempt's lease.
pub(super) struct StoreHandOver<'a, S> {
    store: &'a S,
    lease: &'a OpLease,
}

#[async_trait::async_trait]
impl<S: Store> HandOverLog for StoreHandOver<'_, S> {
    async fn record_hand_over(&self) -> Result<(), String> {
        self.store
            .record_hand_over(self.lease)
            .await
            .map_err(|err| err.to_string())
    }
}

/// What one attempt at a claimed send came to, with its outcome recorded under the lease.
pub(super) enum Attempt {
    /// Delivered, and recorded `Succeeded`.
    Delivered(SubmissionReceipt),
    /// The server refused it, or it never reached the server: recorded `Failed`, which the
    /// store parks for a retry or settles.
    Refused(ProviderError),
    /// It may have been delivered: recorded `NeedsConfirmation`, never retried.
    Unknown(ProviderError),
    /// The outcome could not be recorded, because the op is no longer this attempt's:
    /// another worker recovered it while this one was cut off, and the host has since
    /// answered for it. What the attempt saw is carried for the caller.
    Superseded(Result<SubmissionReceipt, ProviderError>),
}

/// Runs one attempt at the send `leased` holds: the provider call, under a hand-over recorded
/// through the outbox and a lease renewed until it returns, then the outcome recorded.
pub(super) async fn attempt<P, S>(
    provider: &P,
    store: &S,
    account: &AccountId,
    leased: &LeasedPendingOp,
    draft: &Draft,
    ttl: Duration,
) -> Result<Attempt, SyncError>
where
    P: Provider,
    S: Store,
{
    let log = StoreHandOver {
        store,
        lease: &leased.lease,
    };
    let hand_over = HandOver::new(&log);
    let sent = holding(
        store,
        &leased.lease,
        ttl,
        provider.submit_email(account, draft, &hand_over),
    )
    .await;
    let outcome = match &sent {
        Ok(receipt) => PendingOutcome::Succeeded {
            provider_key: receipt.email_key.clone(),
        },
        // An ambiguous send is parked for confirmation, never recorded as a retryable
        // failure: the outbox must not risk putting it in front of its recipients twice.
        Err(err) if err.requires_confirmation() => PendingOutcome::NeedsConfirmation {
            detail: err.detail().to_owned(),
        },
        Err(err) => PendingOutcome::Failed {
            class: err.class(),
            retry_after: err.retry_after(),
        },
    };
    match store.mark_pending_op(&leased.lease, outcome).await {
        Ok(()) => {}
        Err(StoreError::StaleLease) => return Ok(Attempt::Superseded(sent)),
        Err(err) => return Err(err.into()),
    }
    Ok(match sent {
        Ok(receipt) => Attempt::Delivered(receipt),
        Err(err) if err.requires_confirmation() => Attempt::Unknown(err),
        Err(err) => Attempt::Refused(err),
    })
}

/// Sends `draft` through the outbox: durable op → claim → provider submit → record.
///
/// On a provider failure the op is recorded `Failed` (with the failure class) or, when the
/// message may have been delivered, `NeedsConfirmation`, and the error is returned. A failed
/// send stays in the outbox's queue for the host to send again or withdraw.
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

    let receipt = match attempt(provider, store, account, &leased, draft, ttl).await? {
        Attempt::Delivered(receipt) | Attempt::Superseded(Ok(receipt)) => receipt,
        Attempt::Refused(err) | Attempt::Unknown(err) | Attempt::Superseded(Err(err)) => {
            return Err(SyncError::Provider(err));
        }
    };
    Ok(SubmitOutcome {
        op: leased.id,
        email_key: receipt.email_key,
        message_id: receipt.message_id,
        sent_copy: receipt.sent_copy,
        sent_copy_keywords: receipt.sent_copy_keywords,
    })
}
