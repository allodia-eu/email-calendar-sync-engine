//! Outbox-mediated mail submission.
//!
//! A send is **durable before any provider side effect** (`north-star.md` Write
//! Contract): [`submit_mail`] records a [`PendingOp`] carrying the draft (idempotent
//! by `Message-ID`), claims it under a fenced [`OpLease`](engine_store::OpLease),
//! performs the provider call, and records the outcome under that lease. The
//! generated `Message-ID` is stamped on the op so the sent copy reconciles when it
//! later syncs back.
//!
//! This is the thin step-4 driver — one op, claimed and resolved inline. The
//! background worker that drains the outbox, honors `depends_on` chains, and parks
//! ambiguous SMTP sends in `NeedsConfirmation` (step 5) is the later orchestrator.

use core::time::Duration;

use engine_core::ids::{AccountId, MessageIdHeader, ProviderKey};
use engine_core::write::{IdempotencyKey, PendingOp, PendingOpId, PendingOutcome, ResourceKey};
use engine_provider::{Draft, Provider};
use engine_store::{LeaseRequest, Store, WorkerId};

use crate::SyncError;

/// How many runnable ops a submission claim asks for (it resolves only its own).
const CLAIM_LIMIT: usize = 16;

/// The result of a successful submission through the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitOutcome {
    /// The durable op that recorded the send.
    pub op: PendingOpId,
    /// The provider key of the sent message (for reconciliation/threading).
    pub email_key: ProviderKey,
    /// The `Message-ID` that was sent.
    pub message_id: MessageIdHeader,
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
    let op_id = store
        .enqueue_pending_op(
            account.clone(),
            PendingOp::new(idempotency, resource, payload),
        )
        .await?;

    // Claim it under a fenced op lease.
    let req = LeaseRequest::new(worker, ttl);
    let leased = store
        .claim_pending_ops(account.clone(), req, CLAIM_LIMIT)
        .await?
        .into_iter()
        .find(|op| op.id == op_id)
        .ok_or_else(|| SyncError::Outbox(format!("enqueued op {op_id:?} was not claimable")))?;

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
            Ok(SubmitOutcome {
                op: op_id,
                email_key: receipt.email_key,
                message_id: receipt.message_id,
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
