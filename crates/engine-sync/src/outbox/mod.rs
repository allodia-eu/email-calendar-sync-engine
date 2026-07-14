//! Outbox-mediated writes: mail submission and edits ([`mail`]), and calendar writes
//! ([`calendar`]).
//!
//! Every write is **durable before any provider side effect** (`north-star.md` Write
//! Contract): the driver records a [`PendingOp`] carrying the request, claims it under a
//! fenced [`OpLease`](engine_store::OpLease) (the shared [`enqueue_and_claim`]), performs
//! the provider call, and records the outcome under that lease. Mail stamps the generated
//! `Message-ID` so the sent copy reconciles when it later syncs back; a calendar write
//! records the event key so the next sync reconciles the new revision.
//!
//! **The payload is the intent, not the rendered bytes.** A calendar patch stores the
//! `EventEdit` — which occurrence, and what changed — rather than the document it produced,
//! so a retry after a conflict can re-apply it to a *freshly fetched* base instead of
//! re-sending an edit built on a copy the server has moved past. Which adapter renders it,
//! and how, is not the outbox's business.
//!
//! These are the thin per-op drivers — one op, claimed and resolved inline. The background
//! worker that drains the outbox and honors `depends_on` chains is the later orchestrator.

mod calendar;
mod mail;

use core::time::Duration;

pub use calendar::{
    CalendarWriteOutcome, create_calendar_event, delete_calendar_event, patch_calendar_event,
    put_calendar_document,
};
use engine_core::{
    ids::AccountId,
    write::{PendingOp, PendingOutcome},
};
use engine_store::{LeaseRequest, LeasedPendingOp, Store, WorkerId};
pub use mail::{MailEditOutcome, SubmitOutcome, edit_mail, submit_mail};

use crate::SyncError;

/// How many runnable ops a claim asks for (each driver resolves only its own).
const CLAIM_LIMIT: usize = 16;

/// Durably records `op` (idempotent by its key) and claims it under a fenced lease,
/// returning the leased op ready to resolve. The shared head of every outbox driver
/// (`store-and-sync.md`): enqueue → claim, with the same fencing discipline as sync.
///
/// This is the **thin inline** primitive (the precedent `submit_mail` established): it
/// enqueues an op and claims it *right now* to resolve it in the same call. It is not the
/// background outbox worker, so it inherits two limitations the worker will remove (the
/// worker claims runnable ops in id order and resolves whatever it gets): it claims a
/// bounded [`CLAIM_LIMIT`] batch and then finds its own op, so it errors if the account
/// already has ≥`CLAIM_LIMIT` older runnable ops; and a just-enqueued op whose
/// `resource_key` is already held by a live in-flight op is correctly *deferred* by the
/// store (not returned), which surfaces here as an error rather than a wait. Both mean the
/// inline driver assumes low outbox contention; under real contention the orchestrator's
/// worker is the right driver.
async fn enqueue_and_claim<S: Store>(
    store: &S,
    account: &AccountId,
    worker: WorkerId,
    ttl: Duration,
    op: PendingOp,
) -> Result<LeasedPendingOp, SyncError> {
    let op_id = store.enqueue_pending_op(account.clone(), op).await?;
    let req = LeaseRequest::new(worker, ttl);
    store
        .claim_pending_ops(account.clone(), req, CLAIM_LIMIT)
        .await?
        .into_iter()
        .find(|op| op.id == op_id)
        .ok_or_else(|| SyncError::Outbox(format!("enqueued op {op_id:?} was not claimable")))
}

/// Records a failed write outcome with its classification and backoff hint.
///
/// Every calendar write is safe to retry — `PUT`/`DELETE` are idempotent HTTP methods (RFC
/// 7231 §4.2.2), a JMAP `/set` addresses the object by id, and the revision guard makes a
/// retry self-correcting — so, unlike an SMTP send whose post-`DATA` ack can be lost
/// ambiguously, a failed calendar write has no `NeedsConfirmation` case: every failure is a
/// plain classified `Failed`. The mail *edit* path shares this for the same reason
/// (`imap-smtp.md`); only [`submit_mail`] branches.
async fn record_failure<S: Store>(
    store: &S,
    leased: &LeasedPendingOp,
    err: &engine_provider::ProviderError,
) -> Result<(), SyncError> {
    store
        .mark_pending_op(
            &leased.lease,
            PendingOutcome::Failed {
                class: err.class(),
                retry_after: err.retry_after(),
            },
        )
        .await?;
    Ok(())
}
